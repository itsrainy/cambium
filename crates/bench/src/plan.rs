//! A run plan: which `cambium-bench` invocations a run is, in order.
//!
//! One definition of the full run. The launcher screen composes a [`Plan`]
//! and shows its command lines; `cambium-bench run` takes the same plan from
//! flags and executes it; `tools/bench/run-published.sh` builds the binaries
//! and calls `run`. Nothing else knows the flags a `walk` step takes, so the
//! three cannot drift.
//!
//! Every step is a child `cambium-bench` process — the same one a person
//! would type — so a step that fails leaves its result files and its
//! progress file exactly as an interactive run would, and the next step
//! resumes from them.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result};

use crate::util::revs_match;

/// How big a run is: small is the fifteen-minute check before a real run,
/// medium the laptop-sized slice `--quick` is sugar for, and large the
/// configuration `docs/RESULTS.md` is published from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Size {
    Small,
    Medium,
    Large,
}

impl Size {
    pub const ALL: [Size; 3] = [Size::Small, Size::Medium, Size::Large];

    pub fn name(self) -> &'static str {
        match self {
            Size::Small => "small",
            Size::Medium => "medium",
            Size::Large => "large",
        }
    }

    /// The name, or the one each size had before: `smoke`, `laptop` and
    /// `published` are what `run --size` and the scripts took first.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "smoke" => Some(Size::Small),
            "laptop" => Some(Size::Medium),
            "published" => Some(Size::Large),
            _ => Self::ALL.iter().copied().find(|z| z.name() == s),
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Size::Small => "3 commits, 1 change set, into bench-results-smoke/: checks the machine (~15 min)",
            Size::Medium => "50 commits, 50 change sets, Tango on the first 10 (--quick)",
            Size::Large => {
                "each subject's own history window, 40 change sets: what RESULTS.md publishes (~8 h for all five)"
            }
        }
    }

    /// Where a run of this size writes. The smoke run has its own tree so the
    /// real one neither resumes nor overwrites it.
    pub fn results_dir(self) -> &'static str {
        match self {
            Size::Small => "bench-results-smoke",
            _ => "bench-results",
        }
    }
}

/// One kind of step a run takes, per subject, in this order. `Report` is the
/// per-subject report; the plan always ends with one report over every
/// subject.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum StepKind {
    Fetch,
    Build,
    Walk,
    Report,
}

impl StepKind {
    pub const ALL: [StepKind; 4] = [
        StepKind::Fetch,
        StepKind::Build,
        StepKind::Walk,
        StepKind::Report,
    ];

    /// The one workload, which the launcher lets a person pick.
    pub const WORKLOADS: [StepKind; 1] = [StepKind::Walk];

    pub fn name(self) -> &'static str {
        match self {
            StepKind::Fetch => "fetch",
            StepKind::Build => "build",
            StepKind::Walk => "walk",
            StepKind::Report => "report",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.name() == s)
    }

    /// A step the rest of the subject cannot run without: a subject that
    /// cannot fetch must not cost the other four their run, but neither may
    /// its walk be attempted on a worktree that is not there.
    pub fn gates_subject(self) -> bool {
        matches!(self, StepKind::Fetch | StepKind::Build)
    }
}

/// What a composed run is.
#[derive(Clone, Debug)]
pub struct Plan {
    pub subjects: Vec<String>,
    /// Tool names as `--tools` takes them. The walk step passes them on as
    /// `--peers` with cambium left out ([`Self::peer_flag`]): cambium is the
    /// walk's driver and runs whatever this list says.
    pub tools: Vec<String>,
    /// Which steps to take, per subject. The order is [`StepKind::ALL`]'s
    /// whatever order they are named in.
    pub steps: Vec<StepKind>,
    pub size: Size,
    /// Overrides [`Size::results_dir`] when a caller has one.
    pub results_dir: Option<PathBuf>,
    /// Continue the results already in the results directory: every workload
    /// gets `--resume`. Without it a run over recorded results is refused
    /// ([`Plan::refuse_existing`]) unless they are moved aside first
    /// ([`Plan::move_results_aside`]). A small run always starts its workloads
    /// over, in a tree of its own.
    pub resume: bool,
}

impl Plan {
    /// The full run over `subjects`: every step, and bazel-diff and Tango as
    /// the peers.
    pub fn published(subjects: Vec<String>) -> Self {
        Self {
            subjects,
            tools: vec!["bazel-diff".into(), "tango".into()],
            steps: StepKind::ALL.to_vec(),
            size: Size::Large,
            results_dir: None,
            resume: false,
        }
    }

    /// Where this plan's results go.
    pub fn results(&self) -> PathBuf {
        self.results_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(self.size.results_dir()))
    }

    fn wants(&self, kind: StepKind) -> bool {
        self.steps.contains(&kind)
    }

    /// What each workload is told about a result already recorded. A smoke
    /// run restarts: its three commits must not resume a fuller walk. A
    /// resumed run resumes. Anything else says nothing, so the workload
    /// refuses instead of guessing.
    fn on_existing_flag(&self) -> Option<&'static str> {
        match (self.size, self.resume) {
            (Size::Small, _) => Some("--restart"),
            (_, true) => Some("--resume"),
            _ => None,
        }
    }

    /// The results this plan's workloads would run over, one line per
    /// subject: `envoy: walk`.
    pub fn existing_results(&self) -> Vec<String> {
        let root = self.results();
        let mut out = Vec::new();
        for s in &self.subjects {
            let dir = root.join(crate::session::subject_dir_name(s));
            let found: Vec<&str> = [StepKind::Walk]
                .into_iter()
                .filter(|k| self.wants(*k) && has_result(&dir.join(k.name()), k.name()))
                .map(|k| k.name())
                .collect();
            if !found.is_empty() {
                out.push(format!("{s}: {}", found.join(", ")));
            }
        }
        out
    }

    /// Refuses a run that would start over recorded results without being
    /// told to resume them or to move them aside.
    pub fn refuse_existing(&self) -> Result<()> {
        if self.resume || self.size == Size::Small {
            return Ok(());
        }
        let found = self.existing_results();
        anyhow::ensure!(
            found.is_empty(),
            "{dir} already holds results ({}); pass --resume to continue them, or --fresh to move {dir} aside first",
            found.join("; "),
            dir = self.results().display()
        );
        Ok(())
    }

    /// Moves the results directory to `<dir>-<UTC timestamp>`, so the run
    /// starts with nothing under it. `None` when there was nothing to move.
    pub fn move_results_aside(&self) -> Result<Option<PathBuf>> {
        let dir = self.results();
        let empty = std::fs::read_dir(&dir)
            .map(|mut d| d.next().is_none())
            .unwrap_or(true);
        if empty {
            return Ok(None);
        }
        let stamp = crate::util::rfc3339_now().replace([':', '-'], "");
        let base = format!("{}-{stamp}", dir.display());
        let mut to = PathBuf::from(&base);
        let mut n = 1;
        while to.exists() {
            n += 1;
            to = PathBuf::from(format!("{base}-{n}"));
        }
        std::fs::rename(&dir, &to)
            .with_context(|| format!("moving {} aside to {}", dir.display(), to.display()))?;
        Ok(Some(to))
    }

    /// The command lines, in the order they run: per subject the fetch, the
    /// cold build, the workloads and that subject's report, then one report
    /// over every subject at the end.
    pub fn steps(&self) -> Vec<Invocation> {
        let results = self.results().display().to_string();
        let dir: Vec<&str> = vec!["--results-dir", &results];
        let mut out = Vec::new();
        for s in &self.subjects {
            let step = |kind: StepKind, args: Vec<&str>| Invocation {
                kind,
                subject: Some(s.clone()),
                args: args.iter().map(|a| a.to_string()).collect(),
            };
            if self.wants(StepKind::Fetch) {
                let mut a = vec!["--log", "info", "subjects", "fetch", s];
                a.extend(&dir);
                out.push(step(StepKind::Fetch, a));
            }
            if self.wants(StepKind::Build) {
                let mut a = vec!["--log", "info", "subjects", "build", s];
                a.extend(&dir);
                out.push(step(StepKind::Build, a));
            }
            if self.wants(StepKind::Walk) {
                let tier = match self.size {
                    Size::Large => "full",
                    _ => "correctness",
                };
                // Peers only: cambium is the walk's driver, not a peer.
                let peers = self.peer_flag();
                let mut a = vec!["--log", "info", "walk", s, "--tier", tier];
                if !peers.is_empty() {
                    a.extend(["--peers", &peers]);
                }
                a.extend(&dir);
                match self.size {
                    Size::Small => a.extend(["--steps", "3"]),
                    Size::Medium => a.extend(["--steps", "50"]),
                    Size::Large => {}
                }
                if let Some(flag) = self.on_existing_flag() {
                    a.push(flag);
                }
                out.push(step(StepKind::Walk, a));
            }
            if self.wants(StepKind::Report) {
                let mut a = vec!["report", s];
                a.extend(&dir);
                out.push(step(StepKind::Report, a));
            }
        }
        let mut a = vec!["report"];
        a.extend(&dir);
        out.push(Invocation {
            kind: StepKind::Report,
            subject: None,
            args: a.iter().map(|s| s.to_string()).collect(),
        });
        out
    }

    /// The `--peers` value: the configured tools without cambium.
    fn peer_flag(&self) -> String {
        self.tools
            .iter()
            .map(String::as_str)
            .filter(|t| *t != crate::tools::cambi::NAME)
            .collect::<Vec<_>>()
            .join(",")
    }

    pub fn is_runnable(&self) -> bool {
        !self.subjects.is_empty() && self.steps.iter().any(|s| StepKind::WORKLOADS.contains(s))
    }
}

/// One child process: the argument vector, without the program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub kind: StepKind,
    /// The subject the step is for; `None` for the final report.
    pub subject: Option<String>,
    pub args: Vec<String>,
}

impl Invocation {
    /// What the screen shows and what a person could paste.
    pub fn command_line(&self) -> String {
        format!("cambium-bench {}", self.args.join(" "))
    }

    /// A short `<subject> <step>` for a log line.
    pub fn label(&self) -> String {
        match &self.subject {
            Some(s) => format!("{s} {}", self.kind.name()),
            None => "final report".to_string(),
        }
    }
}

/// The steps of a plan, handed out one at a time, with the rule that a failed
/// gating step (a fetch, a build) skips the rest of that subject.
///
/// Both drivers — the launcher's screen and `cambium-bench run` — pull from
/// one of these, so the skip rule is written once.
#[derive(Debug)]
pub struct Queue {
    pending: VecDeque<Invocation>,
    skipping: Option<String>,
    pub total: usize,
}

impl Queue {
    pub fn new(plan: &Plan) -> Self {
        let steps = plan.steps();
        Self {
            total: steps.len(),
            pending: steps.into(),
            skipping: None,
        }
    }

    /// The next step to run, skipping past a subject whose gate failed.
    pub fn pop(&mut self) -> Option<Invocation> {
        loop {
            let step = self.pending.pop_front()?;
            match (&self.skipping, &step.subject) {
                (Some(skip), Some(s)) if skip == s => continue,
                _ => {
                    self.skipping = None;
                    return Some(step);
                }
            }
        }
    }

    /// Tells the queue how a step went.
    pub fn finished(&mut self, step: &Invocation, ok: bool) {
        if !ok && step.kind.gates_subject() {
            self.skipping = step.subject.clone();
        }
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// Runs a plan to the end as children of `exe`, stdio inherited, one `===`
/// line per step. Answers with how many steps failed; a failed step never
/// stops the run, a failed gate skips its subject.
pub fn run_plan(plan: &Plan, exe: &Path) -> Result<usize> {
    let mut queue = Queue::new(plan);
    let mut failures = 0usize;
    while let Some(step) = queue.pop() {
        println!("=== {} {} ===", clock(), step.label());
        let status = Command::new(exe)
            .args(&step.args)
            .stdin(Stdio::null())
            .status()
            .with_context(|| format!("starting {}", step.command_line()))?;
        let ok = status.success();
        if !ok {
            failures += 1;
            println!("FAILED {}", step.label());
        }
        queue.finished(&step, ok);
    }
    Ok(failures)
}

/// Whether `dir` holds a `<workload>-*.json` result file.
fn has_result(dir: &Path, workload: &str) -> bool {
    let prefix = format!("{workload}-");
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten().any(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".json"))
            })
        })
        .unwrap_or(false)
}

/// `HH:MM:SS`, UTC, for a step line.
fn clock() -> String {
    let now = crate::util::rfc3339_now();
    now.get(11..19).unwrap_or(&now).to_string()
}

/// The preflight a published run refuses without: both binaries built from
/// the tree at `sha`.
///
/// Twice a run went on for hours with a binary that was not what the log said
/// it was — a resumed result file, then a stale build — so the driver script
/// compared shas; this is that check, on both the product under measurement
/// and the harness itself.
pub fn require_sha(cambi_version: &str, expected: &str) -> Result<()> {
    let expected = expected.trim();
    anyhow::ensure!(!expected.is_empty(), "--require-sha needs a sha");
    let bin = version_sha(cambi_version).unwrap_or_default();
    anyhow::ensure!(
        revs_match(&bin, expected),
        "REFUSING: cambi is built from '{}' but the tree is at '{expected}'; rebuild first",
        if bin.is_empty() { "?" } else { &bin }
    );
    let own = crate::util::tool_sha();
    anyhow::ensure!(
        revs_match(own, expected),
        "REFUSING: cambium-bench is built from '{own}' but the tree is at '{expected}'; rebuild first"
    );
    Ok(())
}

/// The `(sha)` in `cambi 0.1.0 (abc1234)`.
pub fn version_sha(version: &str) -> Option<String> {
    let inner = version.rsplit_once('(')?.1.split_once(')')?.0.trim();
    (!inner.is_empty() && inner.chars().all(|c| c.is_ascii_hexdigit())).then(|| inner.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(size: Size) -> Plan {
        Plan {
            size,
            ..Plan::published(vec!["bazel".into()])
        }
    }

    fn lines(p: &Plan) -> Vec<String> {
        p.steps().iter().map(|s| s.command_line()).collect()
    }

    /// The published size is the run that produced the published numbers:
    /// these command lines are the contract `run-published.sh` used to spell
    /// out by hand.
    #[test]
    fn the_published_size_is_the_published_run() {
        assert_eq!(
            lines(&plan(Size::Large)),
            [
                "cambium-bench --log info subjects fetch bazel --results-dir bench-results",
                "cambium-bench --log info subjects build bazel --results-dir bench-results",
                "cambium-bench --log info walk bazel --tier full --peers bazel-diff,tango --results-dir bench-results",
                "cambium-bench report bazel --results-dir bench-results",
                "cambium-bench report --results-dir bench-results",
            ]
        );
    }

    #[test]
    fn the_smoke_size_has_its_own_tree_and_three_commits() {
        let got = lines(&plan(Size::Small));
        assert!(
            got.iter()
                .all(|l| l.contains("--results-dir bench-results-smoke")),
            "{got:?}"
        );
        assert!(got[2].contains("--tier correctness"), "{}", got[2]);
        assert!(got[2].ends_with("--steps 3 --restart"), "{}", got[2]);
    }

    #[test]
    fn the_laptop_size_is_the_quick_sugar_and_nothing_else() {
        let got = lines(&plan(Size::Medium));
        assert!(got[2].contains("--tier correctness"), "{}", got[2]);
        assert!(got[2].ends_with("--steps 50"), "{}", got[2]);
        assert!(got[0].contains("bench-results"), "{}", got[0]);
    }

    #[test]
    fn resume_and_step_filters_narrow_the_run() {
        let p = Plan {
            steps: vec![StepKind::Walk, StepKind::Report],
            resume: true,
            ..plan(Size::Large)
        };
        let got = lines(&p);
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(
            got[0].contains(" walk bazel ") && got[0].ends_with("--resume"),
            "{}",
            got[0]
        );
        assert!(!got.iter().any(|l| l.contains(" build ")), "{got:?}");
        // A smoke run starts over whatever it is told.
        let smoke = lines(&Plan {
            resume: true,
            ..plan(Size::Small)
        });
        assert!(smoke[2].ends_with("--restart"), "{}", smoke[2]);
        assert!(!smoke.iter().any(|l| l.contains("--resume")), "{smoke:?}");
        // Told nothing, the workloads are told nothing, and refuse on their own.
        assert!(!lines(&plan(Size::Large))
            .iter()
            .any(|l| l.contains("--resume") || l.contains("--restart")),);
        assert!(
            got[1].starts_with("cambium-bench report bazel"),
            "{}",
            got[1]
        );
        assert!(p.is_runnable());
        assert!(!Plan {
            steps: vec![StepKind::Fetch],
            ..plan(Size::Large)
        }
        .is_runnable());
    }

    /// Recorded results stop a run until it is told what to do with them.
    #[test]
    fn a_run_over_recorded_results_refuses_resumes_or_moves_them_aside() {
        let dir = tempfile::tempdir().unwrap();
        let results = dir.path().join("bench-results");
        std::fs::create_dir_all(results.join("bazel/walk")).unwrap();
        std::fs::write(results.join("bazel/walk/walk-abc.json"), "{}").unwrap();
        let p = Plan {
            results_dir: Some(results.clone()),
            ..plan(Size::Large)
        };
        assert_eq!(p.existing_results(), ["bazel: walk"]);
        let err = p.refuse_existing().unwrap_err().to_string();
        assert!(err.contains("--resume") && err.contains("--fresh"), "{err}");
        assert!(Plan {
            resume: true,
            ..p.clone()
        }
        .refuse_existing()
        .is_ok());
        let build_only = Plan {
            steps: vec![StepKind::Build],
            ..p.clone()
        };
        assert!(
            build_only.existing_results().is_empty(),
            "only the workloads the plan runs count"
        );

        let moved = p.move_results_aside().unwrap().expect("something to move");
        assert!(!results.exists());
        assert!(moved.join("bazel/walk/walk-abc.json").exists());
        assert!(p.refuse_existing().is_ok());
        assert_eq!(
            p.move_results_aside().unwrap(),
            None,
            "nothing left to move"
        );
    }

    #[test]
    fn each_step_is_its_own_invocation_and_no_tools_passes_no_peers() {
        let p = Plan {
            steps: vec![
                StepKind::Fetch,
                StepKind::Build,
                StepKind::Walk,
                StepKind::Report,
            ],
            tools: vec![],
            ..plan(Size::Medium)
        };
        let got = lines(&p);
        assert!(
            !got.iter()
                .any(|l| l.contains(" build ") && l.contains(" walk ")),
            "{got:?}"
        );
        // No peers named: the walk runs cambium alone, with no --peers flag.
        assert!(
            got[2].contains(" walk ") && !got[2].contains("--peers"),
            "{}",
            got[2]
        );
    }

    #[test]
    fn a_plan_with_no_subject_or_no_workload_runs_nothing() {
        assert!(!Plan {
            subjects: vec![],
            ..plan(Size::Medium)
        }
        .is_runnable());
        assert!(!Plan {
            steps: vec![],
            ..plan(Size::Medium)
        }
        .is_runnable());
        assert!(plan(Size::Medium).is_runnable());
    }

    /// A subject whose fetch fails loses its own run and nobody else's.
    #[test]
    fn a_failed_gate_skips_the_rest_of_its_subject_only() {
        let p = Plan::published(vec!["a".into(), "b".into()]);
        let mut q = Queue::new(&p);
        assert_eq!(q.total, 9);
        let fetch_a = q.pop().unwrap();
        assert_eq!(
            (fetch_a.kind, fetch_a.subject.as_deref()),
            (StepKind::Fetch, Some("a"))
        );
        q.finished(&fetch_a, false);
        let next = q.pop().unwrap();
        assert_eq!(
            (next.kind, next.subject.as_deref()),
            (StepKind::Fetch, Some("b"))
        );
        q.finished(&next, true);
        let build_b = q.pop().unwrap();
        assert_eq!(build_b.kind, StepKind::Build);
        // A failed workload does not skip the report that follows it.
        q.finished(&build_b, true);
        let walk = q.pop().unwrap();
        assert_eq!(walk.kind, StepKind::Walk);
        q.finished(&walk, false);
        assert_eq!(q.pop().unwrap().kind, StepKind::Report);
        let last = q.pop().unwrap();
        assert_eq!((last.kind, last.subject), (StepKind::Report, None));
        assert!(q.pop().is_none());
    }

    #[test]
    fn the_sha_preflight_reads_the_version_line() {
        assert_eq!(
            version_sha("cambi 0.1.0 (abc1234)").as_deref(),
            Some("abc1234")
        );
        assert_eq!(version_sha("cambi 0.1.0 (unknown)"), None);
        assert_eq!(version_sha("cambi 0.1.0"), None);
        let own = crate::util::tool_sha();
        if own != "unknown" {
            assert!(require_sha(&format!("cambi 0.1.0 ({own})"), own).is_ok());
        }
        let e = require_sha("cambi 0.1.0 (abc1234)", "def5678")
            .unwrap_err()
            .to_string();
        assert!(e.contains("REFUSING") && e.contains("abc1234"), "{e}");
        assert!(require_sha("cambi 0.1.0 (abc1234)", "").is_err());
        assert!(StepKind::parse("walk") == Some(StepKind::Walk));
        assert!(Size::parse("smoke") == Some(Size::Small));
        assert!(Size::parse("large") == Some(Size::Large));
        assert!(Size::parse("published") == Some(Size::Large));
        assert!(Size::parse("huge").is_none());
    }
}
