//! `cambium-bench replay` as a whole: which change sets to replay, against
//! which index, with which diffs, and what to hold the answers against.
//!
//! [`crate::replay`] is the stage (one diff against one index) and knows
//! nothing about a results directory. This is the orchestration over a
//! finished walk's artefacts: the recorded answers, any archived bases, the
//! paths cache, the re-derivation cache. It runs no Bazel and needs no
//! checkout; a git repository (`--repo`) is optional and makes it exact.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use cambium_diff::Change;
use cambium_engine::externals::WorkspaceText;
use cambium_index::{IndexFile, IndexRef};
use serde::Serialize;

use crate::oracles::rebuild::GraphDiff;
use crate::replay::{
    self, BaseKind, Divergence, PathEntry, PathsCache, RecordedAnswer, RederiveCache,
    ReplayOutcome, ReplayRun,
};
use crate::session::SubjectRun;

/// What `replay` was asked to do.
#[derive(Clone, Debug, Default)]
pub struct ReplayRequest {
    /// `--paths`: explicit repo-relative paths, replayed as one change set.
    pub paths: Vec<String>,
    /// `--change-set base..head`.
    pub change_set: Option<String>,
    /// `--commit sha`: the recorded step whose head it names.
    pub commit: Option<String>,
    /// `--all`: every recorded step.
    pub all: bool,
    /// `--history`: meant, with `--all`, to add the history workload's
    /// commits. A walk's steps are its commits and all of them are recorded,
    /// so it adds nothing and nothing reads it.
    pub history: bool,
    /// `--index`: the index to replay against, outranking everything.
    pub index: Option<PathBuf>,
    /// `--paths-file`: the diff cache; the default is beside the index.
    pub paths_file: Option<PathBuf>,
    /// `--repo`: a git repository to take diffs and workspace text from.
    pub repo: Option<PathBuf>,
    /// `--rederive`: rebuild the externals tables at each base first.
    pub rederive: bool,
    /// `--rederive-dump`: keep each re-derived index on disk here.
    pub rederive_dump: Option<PathBuf>,
    /// `--baseline`: compare against an earlier `--save-baseline`.
    pub baseline: Option<PathBuf>,
    /// `--save-baseline`: write this run's replays here.
    pub save_baseline: Option<PathBuf>,
}

impl ReplayRequest {
    /// `--paths` was given: nothing recorded is being replayed.
    pub fn explicit(&self) -> bool {
        !self.paths.is_empty()
    }
}

/// What a replay of one subject concluded.
#[derive(Clone, Debug, Serialize)]
pub struct ReplayReport {
    pub run: ReplayRun,
    /// The recorded answers, for the rendering to print beside each replay.
    #[serde(skip)]
    pub recorded: Vec<RecordedAnswer>,
    pub divergences: Vec<Divergence>,
    pub skipped: Vec<String>,
    /// What the divergences are against: `baseline` (the `--baseline` file),
    /// `the recorded run`, or `nothing` (an explicit `--paths` has nothing to
    /// be held against).
    pub compared_against: &'static str,
    pub index: PathBuf,
    pub rederived: bool,
}

impl ReplayReport {
    pub fn clean(&self) -> bool {
        self.divergences.is_empty()
    }

    /// How many change sets were replayed against the base the run itself
    /// spliced onto.
    pub fn archived_bases(&self) -> usize {
        self.run
            .outcomes
            .iter()
            .filter(|o| o.base_kind.exact_at_base())
            .count()
    }

    /// The `--format json` document.
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "schema": replay::REPLAY_SCHEMA,
            "subject": self.run.subject,
            "pin": self.run.pin,
            "index": self.index.display().to_string(),
            "archived_bases": self.archived_bases(),
            "compared_against": self.compared_against,
            "rederived": self.rederived,
            "replays": self.run.outcomes,
            "divergences": self.divergences,
            "skipped": self.skipped,
        })
    }
}

/// The stored index at the pin, or `--index`.
pub fn index_path(run: &SubjectRun<'_>, req: &ReplayRequest) -> PathBuf {
    match &req.index {
        Some(p) => p.clone(),
        None => run.index_path(&run.entry.pin),
    }
}

/// Every step the subject's walk recorded, through the schema gate.
pub fn load_recorded(run: &SubjectRun<'_>) -> Result<Vec<RecordedAnswer>> {
    let walk = run.walk_result_path();
    anyhow::ensure!(
        walk.exists(),
        "no walk result under {}: nothing to replay against",
        run.results.display()
    );
    let out = replay::recorded_from_walk(&crate::walk::WalkResult::read(&walk)?);
    anyhow::ensure!(
        !out.is_empty(),
        "the walk at {} recorded no steps",
        walk.display()
    );
    Ok(out)
}

/// Replays one subject.
pub fn replay_subject(run: &SubjectRun<'_>, req: &ReplayRequest) -> Result<ReplayReport> {
    let index_path = index_path(run, req);
    anyhow::ensure!(
        index_path.exists(),
        "no stored index at {}; pull one from the benchmark machine \
         (`cambium-bench pull-results {}`) or pass --index",
        index_path.display(),
        run.entry.id
    );
    let file = IndexFile::open(&index_path)
        .with_context(|| format!("opening {}", index_path.display()))?;
    anyhow::ensure!(
        !req.rederive || req.repo.is_some(),
        "--rederive reads MODULE.bazel, WORKSPACE and the workspace .bzl closure at each base \
         revision, so it needs --repo <git dir>"
    );
    let repo = req
        .repo
        .as_ref()
        .map(|p| cambium_diff::Repo::new(p.clone()));
    let paths_cache = match &req.paths_file {
        Some(p) => Some(PathsCache::read(p)?),
        None => {
            let p = run.paths_cache_path();
            p.exists().then(|| PathsCache::read(&p)).transpose()?
        }
    };
    let recorded = load_recorded(run)?;
    let wanted = wanted(req, &recorded)?;
    let cfg = run.config();

    let mut replays = Replays {
        pin: file.view(),
        // `--index` is named rather than discovered, so a replay against it
        // cannot claim to be at any particular revision and is read as the
        // pin's case.
        pin_kind: if req.index.is_some() {
            BaseKind::Explicit
        } else {
            BaseKind::Pin
        },
        archived: req.index.is_none().then(|| run.results.clone()),
        rederived: RederiveCache::new().with_dump(req.rederive_dump.clone()),
        skipped: Vec::new(),
    };
    let diffs = Diffs {
        explicit: req
            .explicit()
            .then(|| replay::parse_name_status(&req.paths)),
        repo: repo.as_ref(),
        cache: paths_cache.as_ref(),
    };
    let texts: Box<dyn WorkspaceText> = match &repo {
        Some(r) => Box::new(GitText(r.clone())),
        None => Box::new(replay::NoText),
    };
    let mut outcomes: Vec<ReplayOutcome> = Vec::new();
    for want in &wanted {
        let Some(changes) = diffs.changes_for(want, &mut replays.skipped)? else {
            continue;
        };
        if let Some(o) = replays.one(want, &changes, &cfg, texts.as_ref(), req.rederive) {
            outcomes.push(o);
        }
    }

    let run_doc = ReplayRun {
        schema: replay::REPLAY_SCHEMA,
        subject: run.entry.id.clone(),
        pin: run.entry.pin.clone(),
        outcomes,
    };
    if let Some(p) = &req.save_baseline {
        run_doc.write(p)?;
    }
    // Divergences: against a previous replay when one is given (both sides on
    // the same index, so every difference is the engine's), otherwise against
    // what the run recorded.
    let (divergences, compared_against) = match &req.baseline {
        Some(p) => (
            run_doc.divergences_against(&ReplayRun::read(p)?),
            "baseline",
        ),
        None if req.explicit() => (Vec::new(), "nothing"),
        None => {
            let mut d = Vec::new();
            for o in &run_doc.outcomes {
                if let Some(r) = replay::find_recorded(&recorded, &o.base, &o.head) {
                    d.extend(replay::diverge(r, o));
                }
            }
            (d, "the recorded run")
        }
    };
    Ok(ReplayReport {
        run: run_doc,
        recorded,
        divergences,
        skipped: replays.skipped,
        compared_against,
        index: index_path,
        rederived: req.rederive,
    })
}

/// What to replay: an explicit diff, one recorded change set or commit, or
/// all of them.
fn wanted(req: &ReplayRequest, recorded: &[RecordedAnswer]) -> Result<Vec<RecordedAnswer>> {
    if req.explicit() {
        return Ok(vec![RecordedAnswer {
            base: "explicit".into(),
            head: "paths".into(),
            ..Default::default()
        }]);
    }
    if let Some(cs) = &req.change_set {
        let (b, h) = cs
            .split_once("..")
            .context("--change-set takes base..head")?;
        return Ok(vec![replay::find_recorded(recorded, b, h)
            .with_context(|| format!("no recorded change set {cs}"))?
            .clone()]);
    }
    if let Some(sha) = &req.commit {
        return Ok(vec![recorded
            .iter()
            .find(|r| r.head.starts_with(sha.as_str()) || sha.starts_with(r.head.as_str()))
            .with_context(|| format!("no recorded change set or commit at {sha}"))?
            .clone()]);
    }
    if req.all {
        return Ok(recorded.to_vec());
    }
    anyhow::bail!("say what to replay: --paths, --change-set base..head, --commit sha, or --all")
}

/// Where a change set's diff comes from, in order of preference: the command
/// line, a git repository, the paths cache.
struct Diffs<'a> {
    explicit: Option<Vec<Change>>,
    repo: Option<&'a cambium_diff::Repo>,
    cache: Option<&'a PathsCache>,
}

impl Diffs<'_> {
    /// The diff for `want`, or `None` (with a note) when the cache does not
    /// have it.
    fn changes_for(
        &self,
        want: &RecordedAnswer,
        skipped: &mut Vec<String>,
    ) -> Result<Option<Vec<Change>>> {
        if let Some(c) = &self.explicit {
            return Ok(Some(c.clone()));
        }
        if let Some(r) = self.repo {
            return r
                .diff(&want.base, &want.head)
                .with_context(|| format!("git diff {}..{}", want.base, want.head))
                .map(Some);
        }
        if let Some(c) = self.cache {
            return Ok(match c.lookup(&want.base, &want.head) {
                Some(c) => Some(c),
                None => {
                    skipped.push(format!("{}: not in the paths cache", want.key()));
                    None
                }
            });
        }
        anyhow::bail!(
            "the recorded change sets carry no diff; pass --repo <git dir> or --paths-file \
             (`cambium-bench replay --write-paths` on the box writes one)"
        )
    }
}

/// The indexes a subject's replays run against.
struct Replays<'a> {
    pin: IndexRef<'a>,
    pin_kind: BaseKind,
    /// `<results>/<subject>`, when the archived bases under it may be used.
    archived: Option<PathBuf>,
    rederived: RederiveCache,
    skipped: Vec<String>,
}

impl Replays<'_> {
    /// One change set, against the run's own archived base when there is
    /// one, else the pin; re-derived first when asked. `None` when it had to
    /// be skipped, which `skipped` then says.
    fn one(
        &mut self,
        want: &RecordedAnswer,
        changes: &[Change],
        cfg: &cambium_engine::Config,
        texts: &dyn WorkspaceText,
        rederive: bool,
    ) -> Option<ReplayOutcome> {
        // The base the run itself spliced onto, when the retired change-set
        // driver archived one: that is the graph the run walked, where the
        // index at the pin is the graph as of some later commit. `--index` is the one
        // thing that outranks it, because it is an explicit instruction.
        let opened = match &self.archived {
            None => None,
            Some(results) => match replay::archived_base(results, &want.base) {
                None => None,
                Some(path) => match IndexFile::open(&path) {
                    Ok(f) => Some(f),
                    Err(e) => {
                        self.skipped.push(format!(
                            "{}: opening the archived base index {}: {e:#}",
                            want.key(),
                            path.display()
                        ));
                        return None;
                    }
                },
            },
        };
        let base_kind = if opened.is_some() {
            BaseKind::RunsOwn
        } else {
            self.pin_kind
        };
        let base_view = opened.as_ref().map(|f| f.view()).unwrap_or(self.pin);

        // With --rederive the index the stages see is that one with its
        // externals tables rebuilt from the base's own text; without it, the
        // index exactly as the run left it.
        let mut cost = None;
        let replay_view = if rederive {
            match self.rederived.ensure(base_view, &want.base, texts) {
                Ok(ms) => {
                    cost = ms;
                    self.rederived.get(&want.base).unwrap_or(base_view)
                }
                Err(e) => {
                    self.skipped
                        .push(format!("{}: cannot re-derive: {e:#}", want.key()));
                    return None;
                }
            }
        } else {
            base_view
        };
        let mut o = replay::replay_with(replay::ReplayInputs {
            base: replay_view,
            cfg,
            base_rev: &want.base,
            head_rev: &want.head,
            changes,
            texts,
            base_kind,
        });
        o.rederived = rederive;
        o.rederive_ms = cost;
        Some(o)
    }
}

/// `--head-check`: the rebuild oracle's externals half, offline.
///
/// The oracle on the box compares the incrementally applied index against a
/// fresh Bazel build at the pin, and that fresh build is the only artefact
/// that says what the externals tables *are* there. The walk archives it;
/// this opens it, re-derives its tables from the head tree's own text with
/// this build, and compares. `None` when the run archived no head index:
/// quiet by design, since the precheck runs this for every subject and a run
/// from before the head was archived has none.
pub fn head_check_subject(
    run: &SubjectRun<'_>,
    repo: Option<&Path>,
    max_listed: usize,
) -> Result<Option<(PathBuf, GraphDiff)>> {
    let Some(path) = replay::archived_head(&run.results, &run.entry.pin) else {
        tracing::info!(subject = %run.entry.id, pin = %run.entry.pin, "no archived head index; nothing to check");
        return Ok(None);
    };
    let repo = repo.context(
        "--head-check re-derives the externals tables from MODULE.bazel, WORKSPACE and the \
         workspace .bzl closure at the pin, so it needs --repo <git dir>",
    )?;
    let texts = GitText(cambium_diff::Repo::new(repo.to_path_buf()));
    let file = IndexFile::open(&path)
        .with_context(|| format!("opening the archived head {}", path.display()))?;
    let diff = replay::head_check(file.view(), &run.entry.pin, &texts, max_listed)
        .with_context(|| format!("re-deriving the externals tables at {}", run.entry.pin))?;
    Ok(Some((path, diff)))
}

/// `--write-paths`: the diff of every recorded change set, from a git
/// repository, as the paths cache a replay away from that repository reads.
///
/// The one definition of `paths-<pin>.json`: the box runs this over its
/// mirror, the laptop reads what it wrote. A change set whose revisions the
/// repository cannot diff (a pin that has since been garbage-collected) is
/// left out, as the shell loop it replaces left it out.
pub fn write_paths_cache(run: &SubjectRun<'_>, repo: &Path, out: Option<&Path>) -> Result<PathBuf> {
    let walk_path = run.walk_result_path();
    let walk = crate::walk::WalkResult::read(&walk_path)
        .with_context(|| format!("reading {}", walk_path.display()))?;
    let repo = cambium_diff::Repo::new(repo.to_path_buf());
    let mut entries = Vec::new();
    for st in &walk.steps {
        match repo.diff(&st.parent, &st.head) {
            Ok(changes) => entries.push(PathEntry {
                base: st.parent.clone(),
                head: st.head.clone(),
                changes: changes.iter().map(name_status).collect(),
            }),
            Err(e) => {
                tracing::warn!(base = %st.parent, head = %st.head, error = %e, "cannot diff; left out of the paths cache");
            }
        }
    }
    let cache = PathsCache {
        schema: replay::REPLAY_SCHEMA,
        repo: run.entry.id.clone(),
        pin: crate::util::versioned_stem(&run.entry.pin, run.version()),
        entries,
    };
    let path = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| run.paths_cache_path());
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, serde_json::to_string(&cache)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// One change as `git diff --name-status` spells it: `M<TAB>path`.
fn name_status(c: &Change) -> String {
    let s = match c.status {
        cambium_diff::Status::Added => 'A',
        cambium_diff::Status::Deleted => 'D',
        cambium_diff::Status::TypeChanged => 'T',
        _ => 'M',
    };
    format!("{s}\t{}", c.path)
}

/// A [`cambium_diff::Repo`] read as workspace text at a revision.
pub struct GitText(pub cambium_diff::Repo);

impl WorkspaceText for GitText {
    fn text_at(&self, rev: &str, path: &str) -> Option<String> {
        self.0
            .read_at(rev, path)
            .and_then(|b| String::from_utf8(b).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorded(base: &str, head: &str) -> RecordedAnswer {
        RecordedAnswer {
            base: base.into(),
            head: head.into(),
            ..Default::default()
        }
    }

    #[test]
    fn what_to_replay_is_one_of_four_things() {
        let rec = vec![
            recorded("aaaa1111", "bbbb2222"),
            recorded("bbbb2222", "cccc3333"),
        ];
        let req = ReplayRequest {
            change_set: Some("aaaa..bbbb".into()),
            ..Default::default()
        };
        assert_eq!(wanted(&req, &rec).unwrap(), vec![rec[0].clone()]);
        let req = ReplayRequest {
            commit: Some("cccc".into()),
            ..Default::default()
        };
        assert_eq!(wanted(&req, &rec).unwrap(), vec![rec[1].clone()]);
        let req = ReplayRequest {
            all: true,
            ..Default::default()
        };
        assert_eq!(wanted(&req, &rec).unwrap().len(), 2);
        let req = ReplayRequest {
            paths: vec!["M\tsrc/a.cc".into()],
            ..Default::default()
        };
        assert_eq!(wanted(&req, &rec).unwrap()[0].base, "explicit");
        assert!(wanted(&ReplayRequest::default(), &rec).is_err());
        assert!(wanted(
            &ReplayRequest {
                change_set: Some("nope".into()),
                ..Default::default()
            },
            &rec
        )
        .is_err());
    }

    #[test]
    fn a_diff_comes_from_the_cache_when_there_is_no_repository() {
        let cache = PathsCache {
            schema: replay::REPLAY_SCHEMA,
            repo: "r".into(),
            pin: "p".into(),
            entries: vec![PathEntry {
                base: "aaaa1111".into(),
                head: "bbbb2222".into(),
                changes: vec!["M\tsrc/a.cc".into()],
            }],
        };
        let diffs = Diffs {
            explicit: None,
            repo: None,
            cache: Some(&cache),
        };
        let mut skipped = Vec::new();
        let got = diffs
            .changes_for(&recorded("aaaa1111", "bbbb2222"), &mut skipped)
            .unwrap()
            .unwrap();
        assert_eq!(got[0].path, "src/a.cc");
        assert!(diffs
            .changes_for(&recorded("x", "y"), &mut skipped)
            .unwrap()
            .is_none());
        assert_eq!(skipped, ["x..y: not in the paths cache"]);
        let none = Diffs {
            explicit: None,
            repo: None,
            cache: None,
        };
        assert!(none.changes_for(&recorded("x", "y"), &mut skipped).is_err());
    }

    #[test]
    fn a_change_is_spelled_as_name_status() {
        assert_eq!(
            name_status(&Change::new(cambium_diff::Status::Added, "a/b")),
            "A\ta/b"
        );
        assert_eq!(
            name_status(&Change::new(cambium_diff::Status::Modified, "c")),
            "M\tc"
        );
        let back = replay::parse_name_status(&["D\tgone".to_string()]);
        assert_eq!(back[0].status, cambium_diff::Status::Deleted);
    }
}
