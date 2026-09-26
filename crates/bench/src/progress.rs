//! `bench-results/<subject>/progress.json`: where a running workload is.
//!
//! A result file says what a run *found*; this says what it is *doing*, and it
//! exists so that following a run is reading a file rather than grepping a log
//! (issue #1). The harness is the only writer and every write is a temp file
//! and a rename, so a reader either sees the previous step or this one and
//! never half of either. `cambium-bench watch` is the reader; it holds no
//! handle on the run, which is what lets it work on a finished run and on
//! results copied off another machine.
//!
//! Every timestamp is RFC 3339 ([`crate::util::rfc3339_now`]), because the
//! watcher computes elapsed and an ETA from them and a bare epoch count is
//! unreadable in a file a person also opens by hand.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use cambium_engine::diag::Diagnostic;
use serde::{Deserialize, Serialize};

pub const PROGRESS_FILE: &str = "progress.json";

/// The progress document's own version, deliberately not
/// [`crate::RESULT_SCHEMA`]: this file is a *liveness* report, and a watcher
/// that cannot read it must degrade to "no progress" rather than refuse a
/// whole run's results the way [`crate::util::read_result`] does.
pub const PROGRESS_SCHEMA: u32 = 1;

/// What a run is doing. `Fetch` and `Build` are the per-subject preparation
/// steps; `History` and `Changes` are the workloads.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    #[default]
    Fetch,
    Build,
    History,
    Changes,
    Walk,
    Done,
    Failed,
}

impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Step::Fetch => "fetch",
            Step::Build => "build",
            Step::History => "history",
            Step::Changes => "changes",
            Step::Walk => "walk",
            Step::Done => "done",
            Step::Failed => "failed",
        }
    }

    /// Whether the run is over, either way.
    pub fn finished(self) -> bool {
        matches!(self, Step::Done | Step::Failed)
    }
}

/// Which tool is running on the current change set, and since when.
///
/// One Tango invocation is minutes, so a progress file that only named the
/// change set would sit unchanged long enough to look hung.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolProgress {
    pub name: String,
    pub started_at: String,
}

/// Where one subject's run is.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Progress {
    #[serde(default)]
    pub schema: u32,
    /// `history`, `changes`, or the preparation verb (`subjects`).
    pub workload: String,
    pub subject: String,
    pub pin: String,
    pub step: Step,
    /// How far along the step is: the commit or change set about to be done,
    /// one-based, and how many there are.
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub of: usize,
    /// When the process started on this subject.
    pub started_at: String,
    /// When the current step started.
    pub step_started_at: String,
    /// The commit, or `base..head`, being worked on. Empty before the first.
    #[serde(default)]
    pub current: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cambi_version: Option<String>,
    /// The diagnostics the last `cambi` document carried, verbatim. They are
    /// the actionable half of what a run said, and in a log they scroll away.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
}

impl Progress {
    pub fn path(subject_dir: &Path) -> PathBuf {
        subject_dir.join(PROGRESS_FILE)
    }

    /// Reads one subject's progress, or `None` when there is no file.
    ///
    /// An unparseable file is `None` too, with a warning: a watcher must not
    /// fail because the run it is watching is newer than it is.
    pub fn read(subject_dir: &Path) -> Option<Self> {
        let path = Self::path(subject_dir);
        let text = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str(&text) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "unreadable progress file");
                None
            }
        }
    }

    /// Atomically, since a reader polls it every second.
    pub fn write(&self, subject_dir: &Path) -> anyhow::Result<()> {
        crate::util::write_json_atomic(&Self::path(subject_dir), self)
    }
}

/// The harness's handle on the progress file.
///
/// Interior mutability, because the workloads hold it through a `&` options
/// struct they also clone and print, and because a progress file is a
/// side-effect of a run rather than part of its answer: nothing here may
/// fail a workload, so a write error is a warning and the run goes on.
#[derive(Debug)]
pub struct Reporter {
    dir: PathBuf,
    state: Mutex<Progress>,
}

impl Reporter {
    /// A reporter for `subject_dir` (`bench-results/<subject>/`), with the
    /// first document already written: a watcher started a second later has
    /// something to show.
    pub fn new(
        subject_dir: &Path,
        workload: &str,
        subject: &str,
        pin: &str,
        step: Step,
        cambi_version: Option<String>,
    ) -> Self {
        let now = crate::util::rfc3339_now();
        let r = Self {
            dir: subject_dir.to_path_buf(),
            state: Mutex::new(Progress {
                schema: PROGRESS_SCHEMA,
                workload: workload.to_string(),
                subject: subject.to_string(),
                pin: pin.to_string(),
                step,
                index: 0,
                of: 0,
                started_at: now.clone(),
                step_started_at: now,
                current: String::new(),
                tool: None,
                last_error: None,
                cambi_version,
                diagnostics: Vec::new(),
            }),
        };
        r.flush();
        r
    }

    fn update(&self, f: impl FnOnce(&mut Progress)) {
        {
            let mut p = self.state.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut p);
        }
        self.flush();
    }

    fn flush(&self) {
        let p = self.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Err(e) = p.write(&self.dir) {
            tracing::warn!(dir = %self.dir.display(), error = format!("{e:#}"), "could not write the progress file");
        }
    }

    /// Enters a step, restarting its clock and clearing the item.
    pub fn step(&self, step: Step) {
        self.update(|p| {
            p.step = step;
            p.step_started_at = crate::util::rfc3339_now();
            p.current = String::new();
            p.tool = None;
        });
    }

    /// How many items the step will do.
    pub fn total(&self, of: usize) {
        self.update(|p| p.of = of);
    }

    /// The item being worked on: `index` one-based, `current` a commit or
    /// `base..head`.
    pub fn item(&self, index: usize, current: &str) {
        self.update(|p| {
            p.index = index;
            p.current = current.to_string();
            p.tool = None;
        });
    }

    /// Which tool is running on the current item, or `None` between tools.
    pub fn tool(&self, name: Option<&str>) {
        self.update(|p| {
            p.tool = name.map(|n| ToolProgress {
                name: n.to_string(),
                started_at: crate::util::rfc3339_now(),
            })
        });
    }

    /// The diagnostics the last `cambi` document carried. An empty document
    /// clears them: they describe the latest answer, not the run's history.
    pub fn diagnostics(&self, ds: &[Diagnostic]) {
        self.update(|p| p.diagnostics = ds.to_vec());
    }

    /// What went wrong, kept on the document for the watcher's footer.
    pub fn error(&self, err: &str) {
        let err = err.to_string();
        self.update(|p| p.last_error = Some(err));
    }

    /// The step failed and the run stopped.
    pub fn failed(&self, err: &str) {
        self.error(err);
        self.step(Step::Failed);
    }

    pub fn done(&self) {
        self.update(|p| {
            p.step = Step::Done;
            p.step_started_at = crate::util::rfc3339_now();
            p.tool = None;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_step_rewrites_the_file_whole() {
        let dir = tempfile::tempdir().unwrap();
        let r = Reporter::new(
            dir.path(),
            "history",
            "envoy",
            "abc",
            Step::History,
            Some("cambi 0.1.0 (abc1234)".into()),
        );
        let first = Progress::read(dir.path()).expect("written on construction");
        assert_eq!(first.schema, PROGRESS_SCHEMA);
        assert_eq!(first.step, Step::History);
        assert_eq!(first.subject, "envoy");
        assert!(first.started_at.contains('T'), "{:?}", first.started_at);

        r.total(50);
        r.item(12, "c3f9a1");
        r.tool(Some("tango"));
        let p = Progress::read(dir.path()).unwrap();
        assert_eq!((p.index, p.of), (12, 50));
        assert_eq!(p.current, "c3f9a1");
        assert_eq!(p.tool.unwrap().name, "tango");

        // A new item forgets the tool that was running on the last one.
        r.item(13, "d4e5f6");
        assert!(Progress::read(dir.path()).unwrap().tool.is_none());

        r.failed("bazel query exit 3");
        let p = Progress::read(dir.path()).unwrap();
        assert_eq!(p.step, Step::Failed);
        assert_eq!(p.last_error.as_deref(), Some("bazel query exit 3"));
        // The error outlives the step, so the footer still has it when the
        // next step starts.
        r.step(Step::Changes);
        assert!(Progress::read(dir.path()).unwrap().last_error.is_some());

        r.done();
        let p = Progress::read(dir.path()).unwrap();
        assert_eq!(p.step, Step::Done);
        assert!(p.step.finished());
    }

    #[test]
    fn the_diagnostics_are_the_last_documents_and_not_a_running_list() {
        use cambium_engine::diag::{Code, Diagnostic};
        let dir = tempfile::tempdir().unwrap();
        let r = Reporter::new(dir.path(), "changes", "bazel", "p", Step::Changes, None);
        r.diagnostics(&[Diagnostic::warning(
            Code::W004BazelrcGlobal,
            ".bazelrc changed and the run went global",
        )]);
        let p = Progress::read(dir.path()).unwrap();
        assert_eq!(p.diagnostics.len(), 1);
        assert_eq!(p.diagnostics[0].code, Code::W004BazelrcGlobal);
        r.diagnostics(&[]);
        assert!(Progress::read(dir.path()).unwrap().diagnostics.is_empty());
    }

    #[test]
    fn an_unreadable_file_is_no_progress_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Progress::read(dir.path()).is_none(), "no file at all");
        std::fs::write(Progress::path(dir.path()), "{\"workload\":").unwrap();
        assert!(Progress::read(dir.path()).is_none());
    }
}
