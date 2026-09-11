//! A run plan: which `cambium-bench` invocations a run is, in order.
//!
//! One definition of the published run. The launcher screen composes a
//! [`Plan`] and shows its command lines; `cambium-bench run` takes the same
//! plan from flags and executes it; `tools/bench/run-published.sh` builds the
//! binaries and calls `run`. Nothing else knows the flags a published `history`
//! or `changes` step takes, so the three cannot drift.
//!
//! Every step is a child `cambium-bench` process — the same one a person
//! would type — so a step that fails leaves its result files and its
//! progress file exactly as an interactive run would, and the next step
//! resumes from them.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result};

/// How big a run is. The three sizes the repository already has: the fifteen
/// minutes before an eight-hour pass, the laptop-sized slice `--quick` is
/// sugar for, and the published run itself.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Size {
    Smoke,
    Laptop,
    Published,
}

impl Size {
    pub const ALL: [Size; 3] = [Size::Smoke, Size::Laptop, Size::Published];

    pub fn name(self) -> &'static str {
        match self {
            Size::Smoke => "smoke",
            Size::Laptop => "laptop",
            Size::Published => "published",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|z| z.name() == s)
    }

    pub fn describe(self) -> &'static str {
        match self {
            Size::Smoke => "3 commits, 1 change set, into bench-results-smoke/ (~15 min)",
            Size::Laptop => "--quick: 50 commits, 50 change sets, Tango on the first 10",
            Size::Published => "the subject's own window and 40 change sets: the published run",
        }
    }

    /// Where a run of this size writes. The smoke run has its own tree so the
    /// real one neither resumes nor overwrites it.
    pub fn results_dir(self) -> &'static str {
        match self {
            Size::Smoke => "bench-results-smoke",
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
    History,
    Changes,
    Report,
}

impl StepKind {
    pub const ALL: [StepKind; 5] = [
        StepKind::Fetch,
        StepKind::Build,
        StepKind::History,
        StepKind::Changes,
        StepKind::Report,
    ];

    /// The two workloads, which the launcher lets a person pick.
    pub const WORKLOADS: [StepKind; 2] = [StepKind::History, StepKind::Changes];

    pub fn name(self) -> &'static str {
        match self {
            StepKind::Fetch => "fetch",
            StepKind::Build => "build",
            StepKind::History => "history",
            StepKind::Changes => "changes",
            StepKind::Report => "report",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.name() == s)
    }

    /// A step the rest of the subject cannot run without: a subject that
    /// cannot fetch must not cost the other four their run, but neither may
    /// its history be attempted on a worktree that is not there.
    pub fn gates_subject(self) -> bool {
        matches!(self, StepKind::Fetch | StepKind::Build)
    }
}

/// What a composed run is.
#[derive(Clone, Debug)]
pub struct Plan {
    pub subjects: Vec<String>,
    /// Tool names as `--tools` takes them. `cambium` is always in the list it
    /// passes, because it is what the others are compared against.
    pub tools: Vec<String>,
    /// Which steps to take, per subject. The order is [`StepKind::ALL`]'s
    /// whatever order they are named in.
    pub steps: Vec<StepKind>,
    pub size: Size,
    /// Overrides [`Size::results_dir`] when a caller has one.
    pub results_dir: Option<PathBuf>,
    /// `--restart` on `history`: redo the walk rather than resume it.
    pub restart_history: bool,
    /// `--restart` on `changes`: compare every change set again.
    pub restart_changes: bool,
}

impl Plan {
    /// The published run over `subjects`: every step, both foils.
    pub fn published(subjects: Vec<String>) -> Self {
        Self {
            subjects,
            tools: vec!["bazel-diff".into(), "tango".into()],
            steps: StepKind::ALL.to_vec(),
            size: Size::Published,
            results_dir: None,
            restart_history: false,
            restart_changes: false,
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
            if self.wants(StepKind::History) {
                let mut a = vec!["--log", "info", "history", s, "--oracle", "rebuild"];
                a.extend(&dir);
                match self.size {
                    Size::Smoke => a.extend(["--commits", "3"]),
                    Size::Laptop => a.push("--quick"),
                    Size::Published => {}
                }
                // A smoke run always restarts: its three commits must not
                // resume a fuller walk in the same tree.
                if self.restart_history || self.size == Size::Smoke {
                    a.push("--restart");
                }
                out.push(step(StepKind::History, a));
            }
            if self.wants(StepKind::Changes) {
                let tools = self.tool_flag();
                let mut a = vec![
                    "--log",
                    "info",
                    "changes",
                    s,
                    "--tools",
                    &tools,
                    "--oracle",
                    "rdeps,load",
                ];
                a.extend(&dir);
                match self.size {
                    Size::Smoke => a.extend(["--change-sets", "1"]),
                    Size::Laptop => a.push("--quick"),
                    Size::Published => {}
                }
                if self.restart_changes || self.size == Size::Smoke {
                    a.push("--restart");
                }
                out.push(step(StepKind::Changes, a));
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

    /// The `--tools` value: cambium first and always, since naming it alone is
    /// how a run measures cambium with no foil.
    fn tool_flag(&self) -> String {
        let mut names: Vec<&str> = vec![crate::tools::cambi::NAME];
        names.extend(
            self.tools
                .iter()
                .map(String::as_str)
                .filter(|t| *t != crate::tools::cambi::NAME),
        );
        names.join(",")
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
        same_sha(&bin, expected),
        "REFUSING: cambi is built from '{}' but the tree is at '{expected}'; rebuild first",
        if bin.is_empty() { "?" } else { &bin }
    );
    let own = crate::util::tool_sha();
    anyhow::ensure!(
        same_sha(own, expected),
        "REFUSING: cambium-bench is built from '{own}' but the tree is at '{expected}'; rebuild first"
    );
    Ok(())
}

/// The `(sha)` in `cambi 0.1.0 (abc1234)`.
pub fn version_sha(version: &str) -> Option<String> {
    let inner = version.rsplit_once('(')?.1.split_once(')')?.0.trim();
    (!inner.is_empty() && inner.chars().all(|c| c.is_ascii_hexdigit())).then(|| inner.to_string())
}

/// Two shas name the same commit when one is a prefix of the other.
fn same_sha(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
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
            lines(&plan(Size::Published)),
            [
                "cambium-bench --log info subjects fetch bazel --results-dir bench-results",
                "cambium-bench --log info subjects build bazel --results-dir bench-results",
                "cambium-bench --log info history bazel --oracle rebuild --results-dir bench-results",
                "cambium-bench --log info changes bazel --tools cambium,bazel-diff,tango --oracle rdeps,load --results-dir bench-results",
                "cambium-bench report bazel --results-dir bench-results",
                "cambium-bench report --results-dir bench-results",
            ]
        );
    }

    #[test]
    fn the_smoke_size_has_its_own_tree_and_three_commits() {
        let got = lines(&plan(Size::Smoke));
        assert!(
            got.iter()
                .all(|l| l.contains("--results-dir bench-results-smoke")),
            "{got:?}"
        );
        assert!(got[2].ends_with("--commits 3 --restart"), "{}", got[2]);
        assert!(got[3].ends_with("--change-sets 1 --restart"), "{}", got[3]);
    }

    #[test]
    fn the_laptop_size_is_the_quick_sugar_and_nothing_else() {
        let got = lines(&plan(Size::Laptop));
        assert!(got[2].ends_with("--quick"), "{}", got[2]);
        assert!(got[3].ends_with("--quick"), "{}", got[3]);
        assert!(got[0].contains("bench-results"), "{}", got[0]);
    }

    #[test]
    fn restarts_and_step_filters_narrow_the_run() {
        let p = Plan {
            steps: vec![StepKind::Changes, StepKind::Report],
            restart_changes: true,
            restart_history: true,
            ..plan(Size::Published)
        };
        let got = lines(&p);
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(
            got[0].contains(" changes bazel ") && got[0].ends_with("--restart"),
            "{}",
            got[0]
        );
        assert!(!got.iter().any(|l| l.contains(" history ")), "{got:?}");
        // A smoke run's implied restart and an asked-for one are one flag.
        let smoke = lines(&Plan {
            restart_changes: true,
            restart_history: true,
            ..plan(Size::Smoke)
        });
        assert_eq!(smoke[2].matches("--restart").count(), 1, "{}", smoke[2]);
        assert_eq!(smoke[3].matches("--restart").count(), 1, "{}", smoke[3]);
        assert!(
            got[1].starts_with("cambium-bench report bazel"),
            "{}",
            got[1]
        );
        assert!(p.is_runnable());
        assert!(!Plan {
            steps: vec![StepKind::Fetch],
            ..plan(Size::Published)
        }
        .is_runnable());
    }

    #[test]
    fn one_workload_runs_one_workload_and_cambium_is_always_a_tool() {
        let p = Plan {
            steps: vec![
                StepKind::Fetch,
                StepKind::Build,
                StepKind::Changes,
                StepKind::Report,
            ],
            tools: vec![],
            ..plan(Size::Laptop)
        };
        let got = lines(&p);
        assert!(!got.iter().any(|l| l.contains(" history ")), "{got:?}");
        // Naming no foil measures cambium alone, which is a run, not an error.
        assert!(got[2].contains("--tools cambium "), "{}", got[2]);
    }

    #[test]
    fn a_plan_with_no_subject_or_no_workload_runs_nothing() {
        assert!(!Plan {
            subjects: vec![],
            ..plan(Size::Laptop)
        }
        .is_runnable());
        assert!(!Plan {
            steps: vec![],
            ..plan(Size::Laptop)
        }
        .is_runnable());
        assert!(plan(Size::Laptop).is_runnable());
    }

    /// A subject whose fetch fails loses its own run and nobody else's.
    #[test]
    fn a_failed_gate_skips_the_rest_of_its_subject_only() {
        let p = Plan::published(vec!["a".into(), "b".into()]);
        let mut q = Queue::new(&p);
        assert_eq!(q.total, 11);
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
        let hist = q.pop().unwrap();
        q.finished(&hist, false);
        assert_eq!(q.pop().unwrap().kind, StepKind::Changes);
        q.pop();
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
        assert!(StepKind::parse("changes") == Some(StepKind::Changes));
        assert!(Size::parse("smoke") == Some(Size::Smoke));
    }
}
