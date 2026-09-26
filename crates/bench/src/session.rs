//! A session: what every verb resolves once, before it does anything.
//!
//! The command line names a manifest, a subjects directory, a results
//! directory, a `cambi` and a Bazel; every verb used to resolve those for
//! itself, seven copies of the manifest discovery and four of the subjects
//! directory across the clap files. A [`Session`] is that resolution done
//! once, and a [`SubjectRun`] is one subject inside it: the manifest entry,
//! where its results go, the Bazel version its files are named by, and the
//! state and progress files every workload shares.
//!
//! Two ways in, because half the verbs measure nothing:
//!
//! - [`Session::subject`] reads files a run left behind (`labels`, `report`,
//!   `replay`) on a laptop that need not have a Bazel or a `cambi` at all.
//!   It resolves no binary and creates no directory.
//! - [`Session::run_subject`] is a workload: it resolves the Bazel binary and
//!   creates the subject's results directory, absolute, so that every child
//!   process the workload spawns (in the subject's worktree, in a scratch
//!   worktree) names the same directory.

use std::cell::OnceCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use cambium_engine::Config;

use crate::checkout::{Checkout, SubjectsDir};
use crate::manifest::{Manifest, RepoEntry};
use crate::progress::{Reporter, Step};
use crate::state::State;
use crate::subject::Subject;
use crate::tools::cambi::Cambi;
use crate::util::absolute;
use crate::util::{cache_dir, versioned_name};

/// What a session is opened with: the binary's globals and the flags every
/// verb shares. Every `Option` is "the command line said nothing", and the
/// defaults are the ones the flags document.
#[derive(Clone, Debug, Default)]
pub struct SessionOptions {
    /// Where the manifest is discovered upward from, and what a relative
    /// results directory is relative to.
    pub root: PathBuf,
    /// An explicit `subject_repos.toml`; otherwise discovered.
    pub manifest: Option<PathBuf>,
    /// `--results-dir`: `<results-dir>/<subject>/<workload>/`.
    pub results_dir: PathBuf,
    /// `--subjects-dir`; otherwise `~/.cache/cambium/subjects`.
    pub subjects_dir: Option<PathBuf>,
    /// `--bazel-binary`, overriding the subject's own.
    pub bazel_binary: Option<String>,
    /// `--bazel-version`, overriding the subject's own pin.
    pub bazel_version: Option<String>,
    pub seed: u64,
    pub quick: bool,
    /// `--cambi`; otherwise `cambi` on `PATH`.
    pub cambi: Option<PathBuf>,
}

/// The resolution every verb shares.
pub struct Session {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: Manifest,
    pub subjects: SubjectsDir,
    /// `--results-dir`, made absolute. Not created: an offline verb reads it
    /// and must not leave an empty tree behind on a laptop.
    pub results_root: PathBuf,
    pub bazel_binary: Option<String>,
    pub bazel_version: Option<String>,
    pub seed: u64,
    pub quick: bool,
    cambi_path: Option<PathBuf>,
    cambi: OnceCell<Cambi>,
}

impl Session {
    pub fn open(opts: SessionOptions) -> Result<Self> {
        let (manifest_path, manifest) = match &opts.manifest {
            Some(p) => (p.clone(), Manifest::load(p)?),
            None => Manifest::discover(&opts.root)?,
        };
        tracing::info!(manifest = %manifest_path.display(), "session");
        let subjects = SubjectsDir::new(
            opts.subjects_dir
                .clone()
                .unwrap_or_else(SubjectsDir::default_root),
        );
        let results_root = if opts.results_dir.is_absolute() {
            opts.results_dir.clone()
        } else {
            absolute(&opts.root.join(&opts.results_dir))
        };
        Ok(Self {
            root: opts.root,
            manifest_path,
            manifest,
            subjects,
            results_root,
            bazel_binary: opts.bazel_binary,
            bazel_version: opts.bazel_version,
            seed: opts.seed,
            quick: opts.quick,
            cambi_path: opts.cambi,
            cambi: OnceCell::new(),
        })
    }

    /// The `cambi` under measurement, located on first use with its
    /// `--version` read once.
    ///
    /// Lazily, because half of the verbs measure nothing, and refusing to
    /// print a stored label set because no `cambi` is built would defeat the
    /// point of having written it.
    pub fn cambi(&self) -> Result<&Cambi> {
        if let Some(c) = self.cambi.get() {
            return Ok(c);
        }
        let c = Cambi::locate(self.cambi_path.as_deref())?;
        tracing::info!(cambi = %c.path().display(), version = %c.version(), "the binary under measurement");
        Ok(self.cambi.get_or_init(|| c))
    }

    /// One subject, for reading what a run left behind. Emits a synthetic
    /// subject's workspace if it is not there yet (that costs no Bazel);
    /// resolves no binary and creates nothing under the results directory.
    pub fn subject(&self, name: &str) -> Result<SubjectRun<'_>> {
        let subject = Subject::parse(name, &self.manifest)?;
        let entry = subject.entry(&self.subjects)?;
        let results = self.results_root.join(&entry.id);
        Ok(SubjectRun {
            version: self.version_for(&entry),
            session: self,
            subject,
            entry,
            results,
        })
    }

    /// One subject, for a workload: the Bazel binary resolved and made
    /// absolute, the results directory created.
    pub fn run_subject(&self, name: &str) -> Result<SubjectRun<'_>> {
        let mut run = self.subject(name)?;
        let requested = self
            .bazel_binary
            .clone()
            .unwrap_or_else(|| run.entry.bazel.clone());
        run.entry.bazel = resolve_bazel(&requested)?;
        tracing::info!(subject = %run.entry.id, bazel = %run.entry.bazel, "resolved the bazel binary");
        // Absolute, always. Tools and Bazel run as child processes with a
        // different working directory (the subject's worktree, a scratch
        // worktree), so a relative `--results-dir bench-results` would name a
        // different directory in each of them, which is exactly how
        // bazel-diff came to be handed a base worktree that did not exist.
        std::fs::create_dir_all(&run.results)?;
        run.results = absolute(&run.results);
        Ok(run)
    }

    /// One named subject, or every manifest subject that is not skipped.
    pub fn subject_names(&self, one: Option<String>) -> Vec<String> {
        match one {
            Some(s) => vec![s],
            None => self
                .manifest
                .repos
                .iter()
                .filter(|e| !e.skip)
                .map(|e| e.id.clone())
                .collect(),
        }
    }

    /// The flag wins; the manifest's own pin is the fallback for a subject
    /// that needs a specific Bazel and has no `.bazelversion` to say so.
    fn version_for(&self, entry: &RepoEntry) -> Option<String> {
        self.bazel_version
            .clone()
            .or_else(|| entry.bazel_version.clone())
            .filter(|v| !v.is_empty())
    }
}

/// One subject inside a session.
pub struct SubjectRun<'s> {
    pub session: &'s Session,
    /// What the person named, kept so a checkout knows whether it may be
    /// produced on the spot (a synthetic subject) or must already exist.
    pub subject: Subject,
    pub entry: RepoEntry,
    /// `<results>/<subject>`: the state file and the cold indexes, shared by
    /// every workload.
    pub results: PathBuf,
    version: Option<String>,
}

impl SubjectRun<'_> {
    /// The Bazel version the result files are named by, if any.
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// The subject's worktree, emitted and cloned first if it is synthetic.
    pub fn checkout(&self) -> Result<Checkout> {
        self.session
            .subjects
            .open_or_fetch(&self.subject, &self.entry)
    }

    /// The `cambi` this run measures, carrying the subject's bazelisk
    /// environment: a child `cambi` must run the same Bazel the rest of the
    /// comparison does, and on a subject whose `.bazeliskrc` routes through
    /// the Aspect CLI that is not the Bazel `bazelisk` would pick on its own.
    pub fn cambi(&self, checkout: &Checkout) -> Result<Cambi> {
        Ok(self
            .session
            .cambi()?
            .clone()
            .with_env(crate::checkout::bazelisk_env(&checkout.dir, self.version())))
    }

    /// The generated configuration this subject runs under. Generated, not
    /// read from a worktree, so it costs no Bazel and no checkout.
    pub fn config(&self) -> Config {
        self.session
            .subjects
            .config_for(&self.entry, self.version())
    }

    /// `bench-results/<subject>/<workload>/`, not created.
    pub fn workload_path(&self, name: &str) -> PathBuf {
        self.results.join(name)
    }

    /// `bench-results/<subject>/<workload>/`, created.
    pub fn workload(&self, name: &str) -> Result<PathBuf> {
        let dir = self.workload_path(name);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// The state file, as seen by this build: a state another cambium wrote
    /// (older index format, different configuration) is started over.
    pub fn state(&self) -> Result<State> {
        State::load_for(
            &self.results,
            &self.entry.id,
            &self.entry.pin,
            &State::engine_id(&self.config().hash()),
        )
    }

    /// The progress file for this subject, `bench-results/<subject>/progress.json`.
    ///
    /// One per subject and not per workload: it is the run's liveness, and the
    /// watcher's row is a subject.
    pub fn progress(
        &self,
        workload: &str,
        step: Step,
        cambi_version: Option<String>,
    ) -> Arc<Reporter> {
        Arc::new(Reporter::new(
            &self.results,
            workload,
            &self.entry.id,
            &self.entry.pin,
            step,
            cambi_version,
        ))
    }

    /// `<stem>[-<version>].<ext>`: one result file per Bazel version, since
    /// the whole point of naming one is that two can disagree.
    pub fn result_path(&self, dir: &Path, stem: &str, ext: &str) -> PathBuf {
        dir.join(versioned_name(stem, self.version(), ext))
    }

    /// The cold index at `sha`: `bench-results/<subject>/index-<sha>.bin`.
    pub fn index_path(&self, sha: &str) -> PathBuf {
        self.result_path(&self.results, &format!("index-{sha}"), "bin")
    }

    /// The walk result: `walk/walk-<pin>.json`.
    pub fn walk_result_path(&self) -> PathBuf {
        self.result_path(
            &self.workload_path("walk"),
            &format!("walk-{}", self.entry.pin),
            "json",
        )
    }

    /// The cached per-change-set diffs a replay away from the box reads:
    /// `paths-<pin>.json`, beside the index.
    pub fn paths_cache_path(&self) -> PathBuf {
        self.result_path(&self.results, &format!("paths-{}", self.entry.pin), "json")
    }

    /// The packages of the index's busiest targets, for the walk's `hub` tag.
    /// Needs the cold index at the pin, which is a `subjects build`.
    pub fn hub_packages(&self) -> Result<Vec<String>> {
        let state = self.state()?;
        let path = state
            .built
            .get(&self.entry.pin)
            .cloned()
            .unwrap_or_else(|| self.index_path(&self.entry.pin));
        anyhow::ensure!(
            path.exists(),
            "stress change sets need the index at the pin; run \
             `cambium-bench subjects build {}` first",
            self.entry.id
        );
        let data = cambium_index::IndexFile::open(&path)?.deserialize()?;
        let pkgs = crate::walk::steps::hub_packages(data.view());
        tracing::info!(hubs = pkgs.len(), "hub packages from the index");
        Ok(pkgs)
    }

    /// What the walk's step tagger reads besides the commits: the hub
    /// packages and the configured invalidators. The walk and the Skyframe
    /// runner both tag through this, so they see the same steps.
    ///
    /// The hubs are best effort: three of the four content tags need no
    /// index, and a fresh subject has none. Missing hubs cost the `hub` tag
    /// its steps and nothing else.
    pub fn probes(&self) -> crate::walk::Probes {
        let (paths, globs) = self.entry.invalidator_rules();
        crate::walk::Probes {
            hub_packages: self.hub_packages().unwrap_or_default(),
            invalidator_paths: paths,
            invalidator_globs: globs,
        }
    }
}

/// A Bazel binary the benchmark can run: an explicit path as given, otherwise
/// a PATH lookup, otherwise the bazelisk the suite installs for itself.
pub fn resolve_bazel(binary: &str) -> Result<String> {
    if binary.contains('/') {
        let p = expand_tilde(binary);
        anyhow::ensure!(p.exists(), "no bazel binary at {}", p.display());
        return Ok(p.display().to_string());
    }
    if let Some(found) = which(binary) {
        return Ok(found.display().to_string());
    }
    let fallback = cache_dir("bin").join(binary);
    if fallback.exists() {
        return Ok(fallback.display().to_string());
    }
    anyhow::bail!(
        "no {binary} on PATH or in {}; pass --bazel-binary",
        cache_dir("bin").display()
    )
}

/// The first `binary` on `PATH`.
pub fn which(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(binary))
        .find(|c| c.is_file())
}

/// `~/x` as the home directory's `x`; anything else as it is.
pub fn expand_tilde(s: &str) -> PathBuf {
    match s.strip_prefix("~/") {
        Some(rest) => crate::util::home().join(rest),
        None => PathBuf::from(s),
    }
}

/// The directory a subject name files under: `synth:small` is `synth-small`.
pub fn subject_dir_name(name: &str) -> String {
    name.replace(':', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_in(dir: &Path) -> PathBuf {
        let p = dir.join("subject_repos.toml");
        std::fs::write(
            &p,
            format!(
                "[[repo]]\nid = \"bazel\"\nurl = \"u\"\npin = \"{0}\"\nbazel_version = \"7.4.1\"\n\
                 [[repo]]\nid = \"gone\"\nurl = \"u\"\npin = \"{0}\"\nskip = true\n",
                "a".repeat(40)
            ),
        )
        .unwrap();
        p
    }

    fn session_with(dir: &Path, bazel_version: Option<&str>) -> Session {
        std::fs::create_dir_all(dir.join("a/b/c")).unwrap();
        Session::open(SessionOptions {
            root: dir.join("a/b/c"),
            results_dir: PathBuf::from("bench-results"),
            subjects_dir: Some(dir.join("subjects")),
            bazel_version: bazel_version.map(str::to_string),
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn the_manifest_is_discovered_upward_and_the_results_root_is_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let path = manifest_in(dir.path());
        let s = session_with(dir.path(), None);
        assert_eq!(s.manifest_path, path);
        assert!(s.results_root.is_absolute());
        assert!(
            s.results_root.ends_with("a/b/c/bench-results"),
            "{:?}",
            s.results_root
        );
        assert!(
            !s.results_root.exists(),
            "opening a session creates nothing"
        );
        assert_eq!(
            s.subject_names(None),
            ["bazel"],
            "a skipped subject is not offered"
        );
        assert_eq!(s.subject_names(Some("x".into())), ["x"]);
        assert!(Manifest::discover(Path::new("/")).is_err());
    }

    #[test]
    fn a_subject_knows_its_files_without_a_bazel() {
        let dir = tempfile::tempdir().unwrap();
        manifest_in(dir.path());
        let s = session_with(dir.path(), None);
        let run = s.subject("bazel").unwrap();
        let pin = "a".repeat(40);
        assert_eq!(
            run.version(),
            Some("7.4.1"),
            "the manifest's pin is the fallback"
        );
        assert_eq!(run.results, s.results_root.join("bazel"));
        assert!(run
            .walk_result_path()
            .ends_with(format!("bazel/walk/walk-{pin}-7.4.1.json")));
        assert!(run
            .paths_cache_path()
            .ends_with(format!("bazel/paths-{pin}-7.4.1.json")));
        assert!(run.index_path("abc").ends_with("bazel/index-abc-7.4.1.bin"));
        assert!(!run.results.exists(), "an offline subject creates nothing");
        assert!(s.subject("nope").is_err());
    }

    #[test]
    fn the_flag_outranks_the_manifests_bazel_version() {
        let dir = tempfile::tempdir().unwrap();
        manifest_in(dir.path());
        let s = session_with(dir.path(), Some("8.0.0"));
        assert_eq!(s.subject("bazel").unwrap().version(), Some("8.0.0"));
        let s = session_with(dir.path(), Some(""));
        assert_eq!(
            s.subject("bazel").unwrap().version(),
            None,
            "an empty flag is \"whatever the worktree asks for\", the manifest's pin included"
        );
    }

    #[test]
    fn a_bazel_path_is_taken_as_given_and_a_name_is_looked_up() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bazel");
        std::fs::write(&bin, "").unwrap();
        assert_eq!(
            resolve_bazel(&bin.display().to_string()).unwrap(),
            bin.display().to_string()
        );
        assert!(resolve_bazel(&dir.path().join("nope").display().to_string()).is_err());
        assert!(which("sh").is_some());
        assert_eq!(expand_tilde("/x"), PathBuf::from("/x"));
        assert_eq!(expand_tilde("~/x"), crate::util::home().join("x"));
        assert_eq!(subject_dir_name("synth:small"), "synth-small");
    }
}
