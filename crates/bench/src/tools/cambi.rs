//! The `cambi` binary, as the harness runs it.
//!
//! Everything a user would type, the harness types too: `cambi index build`
//! for a cold index, `cambi index update` to bring one forward, `cambi index
//! publish` to upload it, `cambi affected --format json` for the answer. The
//! harness reads the documents back with the engine's own report types — it
//! links `cambium-engine` for those types, and for the offline tools
//! ([`crate::replay`], [`crate::stages`]), never to run the product path
//! in-process.
//!
//! Two consequences, both deliberate:
//!
//! - **What is measured is what a user waits for.** A wall time here includes
//!   process start, the config load, and serialising the answer to JSON. The
//!   engine's own `timings_ms.total` travels beside it, from the document, so
//!   a published table can show both and the difference between them is
//!   visible rather than assumed.
//! - **A change to the surface shows up here.** If a flag is renamed or an
//!   exit code changes meaning, the benchmark fails the way a user's script
//!   would, instead of quietly measuring an API no one runs.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::tools::{run_capture, run_capture_accepting};
use anyhow::{Context as _, Result};
use cambium_engine::report::AffectedReport;

/// The name cambium is recorded under, in `--tools` and in every result file.
pub const NAME: &str = "cambium";

/// `cambi affected` exits 2 when everything is affected; that is an answer,
/// not a failure.
const EXIT_GLOBAL: i32 = 2;

/// The `cambi` binary under measurement.
#[derive(Clone, Debug)]
pub struct Cambi {
    path: PathBuf,
    /// What `cambi --version` said, recorded in every result file so a number
    /// names the binary that produced it.
    version: String,
    /// Extra environment for every invocation (`USE_BAZEL_VERSION`, ...).
    env: Vec<(String, String)>,
}

/// One `cambi affected` run.
#[derive(Clone, Debug)]
pub struct AffectedRun {
    /// The document, as the engine's own type.
    pub report: AffectedReport,
    /// Whole-process wall time: what a user waits for, process start and JSON
    /// serialisation included.
    pub wall: Duration,
    /// The head index the run wrote, when `--out` was asked for and the run
    /// produced one. Absent on a global invalidation, which has no head index
    /// by definition.
    pub index: Option<PathBuf>,
}

impl AffectedRun {
    /// The engine's own total, from the document: the wall time minus
    /// everything the process paid around it.
    pub fn engine_ms(&self) -> u64 {
        self.report.timings_ms.total
    }

    pub fn wall_ms(&self) -> u64 {
        self.wall.as_millis() as u64
    }
}

/// What the harness asks `cambi affected` for.
#[derive(Clone, Debug, Default)]
pub struct AffectedArgs {
    pub base: String,
    pub head: String,
    pub include_manual: bool,
    /// `cambi explain`'s reasons, on the `affected` document. The classifier
    /// needs the per-target reason to explain the largest cambium-side class
    /// on a deletion change set (`cambium_extra_deleted`).
    pub explain: bool,
    /// Publish the base index this run had to compute, so the next run finds
    /// it in the store.
    pub publish_base: bool,
    /// Where the head index goes.
    pub out: Option<PathBuf>,
}

impl AffectedArgs {
    pub fn new(base: &str, head: &str) -> Self {
        Self {
            base: base.to_string(),
            head: head.to_string(),
            ..Default::default()
        }
    }
}

impl Cambi {
    /// `--cambi PATH`, then `$CAMBI`, then `cambi` on `PATH`.
    ///
    /// The first thing it does with what it found is run `--version`, so a
    /// path that is not a runnable `cambi` fails here rather than an hour into
    /// a run.
    pub fn locate(explicit: Option<&Path>) -> Result<Self> {
        Self::locate_from(explicit, std::env::var_os("CAMBI").map(PathBuf::from))
    }

    /// [`Cambi::locate`] with `$CAMBI` already looked up. The process
    /// environment is shared by every test in a binary, so a test of the
    /// environment arm hands the value in rather than setting the variable.
    pub fn locate_from(explicit: Option<&Path>, from_env: Option<PathBuf>) -> Result<Self> {
        if let Some(p) = explicit {
            return Self::at(p).with_context(|| format!("--cambi {}", p.display()));
        }
        if let Some(p) = from_env {
            return Self::at(&p).with_context(|| format!("$CAMBI={}", p.display()));
        }
        Self::at(Path::new("cambi")).context(
            "no cambi binary: pass --cambi <path>, set $CAMBI, or put one on PATH \
             (`cargo build --release -p cambium-cli`)",
        )
    }

    /// The binary at `path`, with its version read.
    pub fn at(path: &Path) -> Result<Self> {
        let out = Command::new(path)
            .arg("--version")
            .output()
            .with_context(|| format!("running {} --version", path.display()))?;
        anyhow::ensure!(
            out.status.success(),
            "{} --version exited with {}",
            path.display(),
            out.status
        );
        let version = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        anyhow::ensure!(
            !version.is_empty(),
            "{} --version printed nothing",
            path.display()
        );
        Ok(Self {
            path: path.to_path_buf(),
            version,
            env: Vec::new(),
        })
    }

    /// Environment every invocation carries: the subject's
    /// `USE_BAZEL_VERSION`, and whatever else keeps bazelisk honest.
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The `cambi x.y.z (sha)` line, recorded with every result.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// An invocation rooted at `ws`, with this run's environment and no stdin
    /// to prompt on — built exactly as a foil tool's is.
    pub fn command(&self, ws: &Path) -> Command {
        let mut cmd = crate::tools::noninteractive_command(&self.path, &self.env);
        cmd.arg("-C").arg(ws);
        cmd
    }

    /// `cambi affected <base>..<head> --format json`, timed end to end.
    pub fn affected(&self, ws: &Path, args: &AffectedArgs) -> Result<AffectedRun> {
        let mut cmd = self.command(ws);
        cmd.arg("affected")
            .arg(format!("{}..{}", args.base, args.head))
            .args(["--format", "json"]);
        if args.include_manual {
            cmd.arg("--include-manual");
        }
        if args.explain {
            cmd.arg("--explain");
        }
        if args.publish_base {
            cmd.arg("--publish-base");
        }
        if let Some(out) = &args.out {
            // A stale file from the previous change set must not be read back
            // as this run's answer.
            let _ = std::fs::remove_file(out);
            cmd.arg("--out").arg(out);
        }
        let what = format!("cambi affected {}..{}", args.base, args.head);
        let out = run_capture_accepting(&mut cmd, &what, |s| {
            s.success() || s.code() == Some(EXIT_GLOBAL)
        })?;
        let report: AffectedReport = serde_json::from_str(&out.stdout).with_context(|| {
            format!(
                "{what}: reading the JSON document it printed\n--- stdout head ---\n{}",
                out.stdout.chars().take(400).collect::<String>()
            )
        })?;
        let index = args.out.clone().filter(|p| p.exists() && !report.global);
        Ok(AffectedRun {
            report,
            wall: out.wall,
            index,
        })
    }

    /// `cambi index build --commit <rev> --out <file>`, timed end to end.
    pub fn index_build(&self, ws: &Path, rev: &str, out: &Path, publish: bool) -> Result<Duration> {
        let mut cmd = self.command(ws);
        cmd.arg("index")
            .arg("build")
            .args(["--commit", rev])
            .arg("--out")
            .arg(out);
        if publish {
            cmd.arg("--publish");
        }
        self.run(&mut cmd, &format!("cambi index build --commit {rev}"))
    }

    /// `cambi index update --from <base> --to <head> --out <file>`, timed end
    /// to end.
    pub fn index_update(
        &self,
        ws: &Path,
        base: &str,
        head: &str,
        out: &Path,
        publish: bool,
    ) -> Result<Duration> {
        let mut cmd = self.command(ws);
        cmd.arg("index")
            .arg("update")
            .args(["--from", base])
            .args(["--to", head])
            .arg("--out")
            .arg(out);
        if publish {
            cmd.arg("--publish");
        }
        self.run(
            &mut cmd,
            &format!("cambi index update --from {base} --to {head}"),
        )
    }

    /// `cambi index publish <file>`.
    pub fn index_publish(&self, ws: &Path, file: &Path) -> Result<Duration> {
        let mut cmd = self.command(ws);
        cmd.arg("index").arg("publish").arg(file);
        self.run(&mut cmd, &format!("cambi index publish {}", file.display()))
    }

    /// Runs one invocation that is expected to succeed, and times it.
    fn run(&self, cmd: &mut Command, what: &str) -> Result<Duration> {
        Ok(run_capture(cmd, what)?.wall)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `cambi` that answers `--version` and records its argv.
    fn fake(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("cambi");
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'cambi 0.1.0 (abc1234)'; exit 0; fi\n{body}\n"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    #[test]
    fn the_version_string_is_read_once_and_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cambi::at(&fake(dir.path(), "exit 0")).unwrap();
        assert_eq!(c.version(), "cambi 0.1.0 (abc1234)");
    }

    #[test]
    fn a_binary_that_is_not_cambi_fails_at_once() {
        let err = Cambi::at(Path::new("/nonexistent/cambi")).unwrap_err();
        assert!(format!("{err:#}").contains("--version"), "{err:#}");
    }

    /// `$CAMBI` is how `tools/bench/run-published.sh` already points the
    /// harness at the binary it just built.
    #[test]
    fn the_environment_names_the_binary_when_no_flag_does() {
        let dir = tempfile::tempdir().unwrap();
        let p = fake(dir.path(), "exit 0");
        // The variable itself is never set: the process environment is
        // shared with every other test in this binary, so the lookup's
        // result is handed in.
        let c = Cambi::locate_from(None, Some(p.clone())).unwrap();
        assert_eq!(c.path(), p);
    }

    #[test]
    fn the_flag_wins_over_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let p = fake(dir.path(), "exit 0");
        let env = Some(PathBuf::from("/nonexistent/cambi"));
        let c = Cambi::locate_from(Some(&p), env).unwrap();
        assert_eq!(c.path(), p);
    }

    #[test]
    fn affected_passes_the_range_and_the_flags_and_times_the_whole_process() {
        let dir = tempfile::tempdir().unwrap();
        let doc = dir.path().join("doc.json");
        std::fs::write(
            &doc,
            serde_json::to_string(&AffectedReport {
                base: "b".into(),
                head: "h".into(),
                timings_ms: cambium_engine::report::Timings {
                    total: 42,
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        let argv = dir.path().join("argv");
        let bin = fake(
            dir.path(),
            &format!(
                "echo \"$@\" > {argv}; cat {doc}; exit 0",
                argv = argv.display(),
                doc = doc.display()
            ),
        );
        let c = Cambi::at(&bin).unwrap();
        let run = c
            .affected(
                dir.path(),
                &AffectedArgs {
                    include_manual: true,
                    explain: true,
                    publish_base: true,
                    ..AffectedArgs::new("b", "h")
                },
            )
            .unwrap();
        let line = std::fs::read_to_string(&argv).unwrap();
        assert!(line.contains("affected b..h"), "{line}");
        assert!(line.contains("--format json"), "{line}");
        assert!(line.contains("--include-manual"), "{line}");
        assert!(line.contains("--publish-base"), "{line}");
        assert_eq!(run.report.base, "b");
        // The engine's own total and the wall time are two numbers, not one.
        assert_eq!(run.engine_ms(), 42);
        assert!(run.index.is_none(), "no --out was asked for");
    }

    /// Exit 2 is "everything is affected", which is an answer: a run that
    /// treated it as a failure would drop every global change set.
    #[test]
    fn a_global_answer_is_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let doc = dir.path().join("doc.json");
        std::fs::write(
            &doc,
            serde_json::to_string(&AffectedReport {
                global: true,
                global_reason: Some("WORKSPACE changed".into()),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        let bin = fake(dir.path(), &format!("cat {}; exit 2", doc.display()));
        let run = Cambi::at(&bin)
            .unwrap()
            .affected(dir.path(), &AffectedArgs::new("b", "h"))
            .unwrap();
        assert!(run.report.global);
    }

    #[test]
    fn any_other_failure_carries_the_stderr_tail() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake(dir.path(), "echo boom >&2; exit 3");
        let err = Cambi::at(&bin)
            .unwrap()
            .affected(dir.path(), &AffectedArgs::new("b", "h"))
            .unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("boom") && s.contains("affected b..h"), "{s}");
    }

    #[test]
    fn index_build_and_update_and_publish_say_what_a_user_would_type() {
        let dir = tempfile::tempdir().unwrap();
        let argv = dir.path().join("argv");
        let bin = fake(
            dir.path(),
            &format!("echo \"$@\" >> {}; exit 0", argv.display()),
        );
        let c = Cambi::at(&bin).unwrap();
        let out = dir.path().join("i.bin");
        c.index_build(dir.path(), "abc", &out, true).unwrap();
        c.index_update(dir.path(), "abc", "def", &out, false)
            .unwrap();
        c.index_publish(dir.path(), &out).unwrap();
        let log = std::fs::read_to_string(&argv).unwrap();
        assert!(log.contains("index build --commit abc"), "{log}");
        assert!(log.contains("--publish"), "{log}");
        assert!(log.contains("index update --from abc --to def"), "{log}");
        assert!(log.contains("index publish"), "{log}");
    }
}
