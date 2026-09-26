//! What a benchmark points at.
//!
//! A *subject* is a Bazel repository a workload runs on, and there are two
//! kinds (docs/METHODOLOGY.md):
//!
//! - [`Subject::Real`] — an entry of `subject_repos.toml`: a URL, a pinned
//!   commit, a history window, and the Bazel version it runs under.
//! - [`Subject::Synthetic`] — a generated graph, named `synth:<preset>` on the
//!   command line, from the presets `cambium-bench synth presets` lists.
//!
//! Every workload accepts either, which is only possible because a synthetic
//! subject is *emitted as a real Bazel workspace with a git history* the first
//! time one is asked for. Once emitted it is described by the same
//! [`RepoEntry`] a real subject is — a local `file://`-shaped URL, a pin, a
//! window — and every step downstream (mirror, worktree, generated
//! `cambium.toml`, history, change sets) is the same code path. The alternative,
//! a second set of workloads that understand in-memory graphs, is how the
//! synthetic and real numbers came to be incomparable in the first place.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::checkout::{git, SubjectsDir};
use crate::manifest::{Manifest, RepoEntry};

/// What a synthetic subject's name starts with: `synth:small`.
pub const SYNTH_PREFIX: &str = "synth:";

/// Commits after the base that an emitted synthetic subject carries.
///
/// It bounds the window a walk over one can take: a synthetic subject with no
/// history is a graph, not a subject.
pub const SYNTH_COMMITS: u64 = 20;

/// The change preset the generated commits are drawn from.
pub const SYNTH_CHANGES: &str = "typical-pr";

/// A repository a workload runs on.
#[derive(Clone, Debug, PartialEq)]
pub enum Subject {
    /// An entry of `subject_repos.toml`.
    Real(RepoEntry),
    /// A generated graph, emitted on demand.
    Synthetic(Synthetic),
}

/// The recipe for a synthetic subject. Two subjects with the same recipe emit
/// byte-identical workspaces, so the emitted tree is cached and reused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Synthetic {
    pub preset: String,
    pub changes: String,
    pub commits: u64,
    pub seed: u64,
}

impl Synthetic {
    pub fn new(preset: impl Into<String>) -> Self {
        Self {
            preset: preset.into(),
            changes: SYNTH_CHANGES.to_string(),
            commits: SYNTH_COMMITS,
            seed: 0,
        }
    }

    /// The filesystem-safe id: `synth-small`.
    pub fn id(&self) -> String {
        format!("synth-{}", self.preset)
    }

    /// The name a person types: `synth:small`.
    pub fn name(&self) -> String {
        format!("{SYNTH_PREFIX}{}", self.preset)
    }

    /// Where the emitted workspace lives under the subjects cache.
    pub fn source_dir(&self, subjects: &SubjectsDir) -> PathBuf {
        subjects.root().join("synthetic").join(format!(
            "{}-{}-{}-{}",
            self.preset, self.changes, self.commits, self.seed
        ))
    }

    /// Emits the workspace if it is not already there, and returns its path
    /// with the shas of its first-parent history, oldest first.
    ///
    /// Idempotent: an existing directory with a git history is reused, so the
    /// second `cambium-bench walk synth:small` of the day costs nothing.
    pub fn emit(&self, subjects: &SubjectsDir) -> Result<(PathBuf, Vec<String>)> {
        let dir = self.source_dir(subjects);
        if dir.join(".git").exists() {
            let shas = first_parent(&dir)?;
            if shas.len() as u64 == self.commits + 1 {
                tracing::debug!(dir = %dir.display(), "reusing the emitted synthetic subject");
                return Ok((dir, shas));
            }
            tracing::warn!(dir = %dir.display(), "re-emitting an incomplete synthetic subject");
            std::fs::remove_dir_all(&dir)?;
        }
        let spec = cambium_synth::GraphSpec::preset(&self.preset)
            .with_context(|| format!("unknown graph preset {:?}", self.preset))?
            .with_seed(self.seed);
        let change = cambium_synth::ChangeSpec {
            seed: self.seed,
            ..cambium_synth::ChangeSpec::preset(&self.changes)
                .with_context(|| format!("unknown change preset {:?}", self.changes))?
        };
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        tracing::info!(
            preset = %self.preset,
            commits = self.commits,
            dir = %dir.display(),
            "emitting a synthetic subject"
        );
        let graph = cambium_synth::generate(&spec);
        let repo = cambium_synth::emit_git_history(graph, &change, self.commits, &dir, false)?;
        Ok((repo.dir, repo.commits))
    }

    /// The manifest entry an emitted synthetic subject looks like.
    pub fn entry(&self, subjects: &SubjectsDir) -> Result<RepoEntry> {
        let (dir, shas) = self.emit(subjects)?;
        let pin = shas
            .last()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{} emitted no commits", self.name()))?;
        Ok(RepoEntry {
            id: self.id(),
            url: dir.display().to_string(),
            pin,
            history_window: shas.len().saturating_sub(1),
            bazel: "bazelisk".to_string(),
            bazel_version: None,
            extra_query_flags: Vec::new(),
            extra_startup_flags: Vec::new(),
            publish_partial: Default::default(),
            invalidators: Vec::new(),
            skip: false,
            notes: format!("synthetic subject from the {} preset", self.preset),
        })
    }
}

impl Subject {
    /// Resolves a name from the command line: a manifest id, or
    /// `synth:<preset>`.
    pub fn parse(name: &str, manifest: &Manifest) -> Result<Self> {
        match name.strip_prefix(SYNTH_PREFIX) {
            Some(preset) => {
                anyhow::ensure!(
                    !preset.is_empty(),
                    "{SYNTH_PREFIX}<preset>: no preset given"
                );
                anyhow::ensure!(
                    cambium_synth::GraphSpec::preset(preset).is_some(),
                    "unknown graph preset {preset:?}; `cambium-bench synth presets` lists them"
                );
                Ok(Self::Synthetic(Synthetic::new(preset)))
            }
            None => Ok(Self::Real(manifest.get(name)?.clone())),
        }
    }

    /// The filesystem-safe id used for results, mirrors and stores.
    pub fn id(&self) -> String {
        match self {
            Self::Real(e) => e.id.clone(),
            Self::Synthetic(s) => s.id(),
        }
    }

    /// The name a person types.
    pub fn name(&self) -> String {
        match self {
            Self::Real(e) => e.id.clone(),
            Self::Synthetic(s) => s.name(),
        }
    }

    pub fn is_synthetic(&self) -> bool {
        matches!(self, Self::Synthetic(_))
    }

    /// The manifest entry for this subject, emitting the workspace first if it
    /// is synthetic.
    pub fn entry(&self, subjects: &SubjectsDir) -> Result<RepoEntry> {
        match self {
            Self::Real(e) => Ok(e.clone()),
            Self::Synthetic(s) => s.entry(subjects),
        }
    }
}

/// The first-parent shas of `dir`, oldest first.
fn first_parent(dir: &Path) -> Result<Vec<String>> {
    let out = git(dir, &["rev-list", "--first-parent", "--reverse", "HEAD"])?;
    Ok(out.lines().map(|l| l.trim().to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        toml::from_str(&format!(
            "[[repo]]\nid = \"bazel\"\nurl = \"u\"\npin = \"{}\"\n",
            "a".repeat(40)
        ))
        .unwrap()
    }

    #[test]
    fn a_name_is_a_manifest_id_or_a_preset() {
        let m = manifest();
        let real = Subject::parse("bazel", &m).unwrap();
        assert!(!real.is_synthetic());
        assert_eq!(real.id(), "bazel");
        assert_eq!(real.name(), "bazel");

        let synth = Subject::parse("synth:small", &m).unwrap();
        assert!(synth.is_synthetic());
        assert_eq!(synth.id(), "synth-small");
        assert_eq!(synth.name(), "synth:small");

        assert!(Subject::parse("nope", &m).is_err());
        assert!(Subject::parse("synth:nope", &m).is_err());
        assert!(Subject::parse("synth:", &m).is_err());
    }

    #[test]
    fn an_emitted_subject_is_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let subjects = SubjectsDir::new(tmp.path().to_path_buf());
        let s = Synthetic {
            commits: 2,
            ..Synthetic::new("tiny")
        };
        let entry = s.entry(&subjects).unwrap();
        assert_eq!(entry.id, "synth-tiny");
        assert_eq!(entry.history_window, 2);
        assert_eq!(entry.pin.len(), 40);
        assert!(Path::new(&entry.url).join("MODULE.bazel").exists());
        // The second call emits nothing and answers with the same pin.
        let again = s.entry(&subjects).unwrap();
        assert_eq!(entry, again);
    }
}
