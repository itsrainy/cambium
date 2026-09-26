//! Fixtures shared by the integration tests of every cambium crate.
//!
//! Each `tests/*.rs` file is its own binary, so anything two of them need has
//! to live in a crate they both depend on — this one, as a dev-dependency
//! only. Nothing here runs Bazel: every engine this crate hands out points at
//! `/nonexistent/bazel`, so a test that accidentally shells out fails loudly
//! instead of passing slowly.
//!
//! The pieces, roughly in the order a test uses them:
//!
//! - [`scratch`], a per-test temporary directory;
//! - [`git`], [`init_repo`], [`commit_all`] and [`write`], for tests that
//!   need a real repository;
//! - [`engine`], [`engine_with`] and [`synthetic_engine`], engines whose
//!   Bazel cannot run, plus [`req`] and [`base`] for driving them;
//! - [`Files`], [`Scanning`] and [`DirQuerier`], stand-ins for the Bazel
//!   query;
//! - [`published_fixture`], a git history with an index published for one of
//!   its commits, for the two command-line contracts.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::Command;

use cambium_engine::query::Rows;
use cambium_engine::{AffectedRequest, Config, Engine, EngineError, PackageQuerier, SynthQuerier};
use cambium_index::{FileText, IndexData, NodeKind, PackageUpdate, RepoMapping, TargetRow};
pub use tempfile::TempDir;

// ------------------------------------------------------------------ scratch

/// A temporary directory of this test's own, deleted when the guard drops.
///
/// Keep the guard alive for as long as the test needs the directory: the
/// `_dir` field of a fixture struct is the usual place. Every test binary
/// used to share one `$TMPDIR/cambium-tests`, which two binaries running in
/// parallel could trip over.
pub fn scratch() -> TempDir {
    tempfile::Builder::new()
        .prefix("cambium-test-")
        .tempdir()
        .expect("a temporary directory")
}

// ---------------------------------------------------------------------- git

/// Runs git in `dir` and returns its trimmed stdout, panicking with stderr on
/// failure — a failed `git init` inside a fixture is a broken test, not a
/// condition worth propagating.
///
/// Used by every test that builds a repository: the engine's `bazelrc_*`,
/// `deleted_build_files`, `revision_workspace`, `tree_keys` and `e2e_bazel`,
/// and the bench crate's `smoke_real_bazel` and `history_synthetic`.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Creates `root` and makes it a git repository with an identity, so commits
/// work on a machine with no global git config (which CI is).
///
/// [`synthetic_engine`] keeps its cache inside the repository, so it has to
/// be invisible to `git add -A` — otherwise a fetched index lands in the next
/// commit and moves the tree, which is precisely what the tree-keying tests
/// measure. `.git/info/exclude` rather than a `.gitignore`, because that file
/// would itself be in the tree.
pub fn init_repo(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "t"]);
    std::fs::write(
        root.join(".git/info/exclude"),
        CACHE_DIR_NAME.to_string() + "/\n",
    )
    .unwrap();
}

/// Where [`synthetic_engine`] puts its cache, relative to the repository root.
pub const CACHE_DIR_NAME: &str = "_cambium_cache";

/// Writes `text` to `root/rel`, creating the parent directories.
pub fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, text).unwrap();
}

/// `git add -A && git commit`, returning the new commit's full sha.
///
/// Empty commits are allowed: the tree-keying tests want two commits with
/// identical content, which is the whole case they are about.
pub fn commit_all(root: &Path, message: &str) -> String {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "--allow-empty", "-m", message]);
    git(root, &["rev-parse", "HEAD"])
}

/// Writes an executable `/bin/sh` script at `path` — `body` after the
/// shebang — and returns once it can be run.
///
/// The returning-once-runnable part is the point. A test binary runs its
/// tests on many threads, and a `Command` in one of them forks while this
/// one still has the file open for writing; the child holds that descriptor
/// until it execs, and an exec of the script in that window fails with
/// `ETXTBSY`, "text file busy". It was seen once, as a stub `cambi` whose
/// `--version` would not start. So the script gets a probe argument that
/// exits before `body`, and this runs it with that argument until the exec
/// goes through — after which no process holds the descriptor and the
/// script never fails to start again.
pub fn executable_script(path: &Path, body: &str) {
    const PROBE: &str = "--cambium-testkit-probe";
    std::fs::write(
        path,
        format!("#!/bin/sh\n[ \"$1\" = {PROBE} ] && exit 0\n{body}\n"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut attempts = 0;
    loop {
        match Command::new(path).arg(PROBE).output() {
            Ok(_) => return,
            // ETXTBSY, by number: its `ErrorKind` is not stable on the MSRV.
            Err(e) if e.raw_os_error() == Some(26) && attempts < 200 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => panic!("running {}: {e}", path.display()),
        }
    }
}

// ------------------------------------------------------------------ engines

/// An engine over `root` with a Bazel that cannot run.
///
/// Every test here supplies its own rows through a [`PackageQuerier`], so the
/// binary path must never be reached; pointing it at a path that does not
/// exist turns "the test accidentally shelled out to Bazel" from a slow
/// success into a loud failure. The cache is put under `root` so the caller's
/// tempdir owns it, rather than the shared default cache directory —
/// [`init_repo`] excludes it from the repository so it cannot reach a tree.
///
/// For tests over a real repository (`bazelrc_diff`, `deleted_build_files`,
/// `tree_keys`, `history_synthetic`, ...); [`engine`] is the same thing over
/// a directory nobody looks at.
pub fn synthetic_engine(root: &Path, cfg: Config) -> Engine {
    Engine::with_parts(
        cfg,
        cambium_diff::Repo::new(root.to_path_buf()),
        cambium_bazel::Bazel::at(root.to_path_buf(), Some("/nonexistent/bazel")),
    )
    .with_cache_dir(root.join(CACHE_DIR_NAME))
}

/// An [`Engine`] that owns the scratch directory it is rooted in.
///
/// It dereferences to the engine, so `engine().full_build_with(..)` reads as
/// before; the directory lives as long as the value does, and no longer.
pub struct Sandbox {
    _dir: TempDir,
    engine: Engine,
}

impl Sandbox {
    /// The engine's workspace and repository root.
    pub fn root(&self) -> &Path {
        self.engine.workspace()
    }
}

impl Deref for Sandbox {
    type Target = Engine;
    fn deref(&self) -> &Engine {
        &self.engine
    }
}

impl DerefMut for Sandbox {
    fn deref_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }
}

/// A default-configured engine over a fresh [`scratch`] directory, for the
/// in-memory tests that never look at the tree (`bzl_closure`,
/// `external_repos`, `incremental`, `explain`, `soundness`, ...).
pub fn engine() -> Sandbox {
    engine_with(Config::default())
}

/// [`engine`] with a configuration of the test's own.
pub fn engine_with(cfg: Config) -> Sandbox {
    let dir = scratch();
    let engine = synthetic_engine(dir.path(), cfg);
    Sandbox { _dir: dir, engine }
}

/// The request the in-memory tests compute `base..head` with: manual targets
/// included, no full build allowed, nothing materialised or published. Build
/// on it with struct update syntax for the odd field a test flips.
pub fn req() -> AffectedRequest {
    AffectedRequest {
        include_manual: true,
        no_full_build: true,
        ..AffectedRequest::new("base", "head")
    }
}

/// A fresh [`engine`] and the index it cold-built at `c0` from `querier`.
pub fn base(querier: &dyn PackageQuerier) -> (Sandbox, IndexData) {
    let e = engine();
    let index = e.full_build_with("c0", querier).unwrap();
    (e, index)
}

// ----------------------------------------------------------------- queriers

/// An in-memory checkout: repo-relative path to file text, served to the
/// engine's `.bzl` scan through [`FileText`].
///
/// The `Scanning` queriers in `bzl_closure`, `bazelrc_flags`,
/// `external_repos` and `external_path_maps` read from one of these.
#[derive(Debug, Default, Clone)]
pub struct Files(pub BTreeMap<String, String>);

impl Files {
    /// Every path in the tree, sorted.
    pub fn paths(&self) -> Vec<String> {
        self.0.keys().cloned().collect()
    }
}

impl FileText for Files {
    fn text(&self, path: &str) -> Option<String> {
        self.0.get(path).cloned()
    }
}

/// A querier whose rows are a function of the package name.
///
/// `rows` answers for one package, `packages` is the universe, and the rest
/// is what the test wants the engine to be able to see: a [`Files`] tree for
/// the load closure, a repository mapping, and whether the tree can be
/// enumerated for the changed-`.bzl` witness. With none of them set it is a
/// querier with no filesystem behind it, which builds an index with no
/// closure — the shape `root_build` and `bzl_closure`'s blind querier use.
pub struct Scanning {
    packages: &'static [&'static str],
    rows: fn(&str) -> PackageUpdate,
    files: Option<Files>,
    repo_mapping: Option<RepoMapping>,
    enumerates_tree: bool,
    workspace: Option<PathBuf>,
    external_rules: Option<String>,
    /// What the repository probe answers, per repository, and which
    /// repositories it has been asked about.
    repo_probes: BTreeMap<String, cambium_bazel::RepoProbe>,
    repo_consumers: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
    /// What `consumer_leaf_packages` answers: indexed repository -> leaf ->
    /// the packages of it the leaf reaches and what it reaches them
    /// through. A repository with no entry answers `None`.
    consumer_leaves: BTreeMap<String, BTreeMap<String, cambium_index::externals::LeafReach>>,
    /// Every `consumer_leaf_packages` question, as `(repo, leaves)`.
    leaf_questions: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    probed: std::sync::Mutex<Vec<String>>,
    /// The `--skykey_filter` of every loading-layer dump this querier was
    /// asked for, in order. A dump is the most expensive thing an incremental
    /// run does on a long-lived server, so a test can ask whether one
    /// happened at all, and for what.
    dumps: std::sync::Mutex<Vec<String>>,
    /// Stand in for a server that has the loading graph's nodes and not its
    /// package values: the first dump comes back with every `PACKAGE` block
    /// removed, and asking the server to load the repository again restores
    /// them. Real Bazel does this after a streamed `//...:*` query (seen on
    /// apollo 7.4.1), and the engine is expected to notice and repair it.
    hides_packages_until_reloaded: std::sync::atomic::AtomicBool,
    /// How many times the engine asked for the repository to be loaded again.
    reloads: std::sync::atomic::AtomicUsize,
    /// The same for the refresh path: from this dump onward (1-based) the
    /// `PACKAGE` blocks are removed, until the engine loads by label. Zero
    /// never hides. A cold build takes dump 1, so `2` leaves the index whole
    /// and breaks the splice's refresh, which is the case worth a test.
    hides_from_dump: std::sync::atomic::AtomicUsize,
    /// How many dumps this querier has served.
    dumps_served: std::sync::atomic::AtomicUsize,
    /// Where a diagnostic the index-building side raises while reading this
    /// querier's checkout goes, so a test can assert on the code.
    diags: cambium_engine::Diagnostics,
}

impl Scanning {
    /// Rows from `rows`, over the universe `packages`, and nothing else.
    pub fn new(packages: &'static [&'static str], rows: fn(&str) -> PackageUpdate) -> Self {
        Self {
            packages,
            rows,
            files: None,
            repo_mapping: None,
            enumerates_tree: false,
            workspace: None,
            external_rules: None,
            repo_probes: BTreeMap::new(),
            repo_consumers: BTreeMap::new(),
            consumer_leaves: BTreeMap::new(),
            leaf_questions: std::sync::Mutex::new(Vec::new()),
            probed: std::sync::Mutex::new(Vec::new()),
            dumps: std::sync::Mutex::new(Vec::new()),
            hides_packages_until_reloaded: std::sync::atomic::AtomicBool::new(false),
            reloads: std::sync::atomic::AtomicUsize::new(0),
            hides_from_dump: std::sync::atomic::AtomicUsize::new(0),
            dumps_served: std::sync::atomic::AtomicUsize::new(0),
            diags: cambium_engine::Diagnostics::new(),
        }
    }

    /// The diagnostics raised while reading this querier's checkout.
    pub fn diags(&self) -> &cambium_engine::Diagnostics {
        &self.diags
    }

    /// Serve every dump from the `n`th onward with its `PACKAGE` blocks
    /// missing, until the engine loads the packages by label.
    pub fn hiding_packages_from_dump(self, n: usize) -> Self {
        self.hides_from_dump
            .store(n, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Serve the first dump with its `PACKAGE` blocks missing, as a real
    /// server does after a streamed universe query, and serve them once the
    /// engine has asked for the repository to be loaded again.
    pub fn hiding_packages_until_reloaded(self) -> Self {
        self.hides_packages_until_reloaded
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// How many times the engine asked Bazel to load the repository again.
    pub fn reloads(&self) -> usize {
        self.reloads.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The filters this querier has been asked to dump, in order. Empty means
    /// no dump ran.
    pub fn dumps(&self) -> Vec<String> {
        self.dumps.lock().unwrap().clone()
    }

    /// Answer the repository probe for `repo` with these packages (`""` is
    /// the repository root), and say whether one of them failed to load.
    pub fn with_repo_probe(
        mut self,
        repo: &str,
        packages: &[&str],
        load_error: bool,
    ) -> Self {
        self.repo_probes.insert(
            repo.to_string(),
            cambium_bazel::RepoProbe {
                repo: repo.to_string(),
                packages: packages.iter().map(|p| p.to_string()).collect(),
                load_error,
            },
        );
        self
    }

    /// Answer `consumer_repos` with: `consumer`'s rules depend on these
    /// packages of the indexed repository `repo`.
    pub fn with_repo_consumers(mut self, repo: &str, consumer: &str, packages: &[&str]) -> Self {
        self.repo_consumers
            .entry(repo.to_string())
            .or_default()
            .insert(
                consumer.to_string(),
                packages.iter().map(|p| p.to_string()).collect(),
            );
        self
    }

    /// Answer `consumer_leaf_packages` for `repo` with: the leaf `leaf`
    /// reaches these packages of it (none at all is "reaches nothing"). A
    /// leaf asked about and never named here is left out of the answer, as
    /// Bazel leaves out a label it could not resolve.
    pub fn with_consumer_leaf(self, repo: &str, leaf: &str, packages: &[&str]) -> Self {
        self.with_consumer_leaf_via(repo, leaf, packages, &[])
    }

    /// [`Scanning::with_consumer_leaf`], and the leaf's closure runs through
    /// `via` outside the repository (`//pkg` for a main-repo package, `@name`
    /// for another repository), which is what a run holds the map's
    /// freshness against (`Consumer::via`).
    pub fn with_consumer_leaf_via(
        mut self,
        repo: &str,
        leaf: &str,
        packages: &[&str],
        via: &[&str],
    ) -> Self {
        self.consumer_leaves
            .entry(repo.to_string())
            .or_default()
            .insert(
                leaf.to_string(),
                cambium_index::externals::LeafReach {
                    packages: packages.iter().map(|p| p.to_string()).collect(),
                    via: via.iter().map(|p| p.to_string()).collect(),
                },
            );
        self
    }

    /// Every leaf question the engine asked, as `(repo, leaves)`, in order.
    pub fn leaf_questions(&self) -> Vec<(String, Vec<String>)> {
        self.leaf_questions.lock().unwrap().clone()
    }

    /// The repositories the engine has probed, in order. Empty means no
    /// probe ran.
    pub fn probed(&self) -> Vec<String> {
        self.probed.lock().unwrap().clone()
    }

    /// Serve `files` as the Starlark source of the load closure.
    pub fn with_files(mut self, files: Files) -> Self {
        self.files = Some(files);
        self
    }

    /// The repository mapping, as `bazel mod dump_repo_mapping ""` prints it.
    pub fn with_repo_mapping(mut self, dump: &str) -> Self {
        self.repo_mapping = Some(RepoMapping::from_dump(dump));
        self
    }

    /// A real directory to answer `workspace()` with, for the reads the
    /// engine does against the checkout itself: the repository-boundary walk
    /// is the one that matters here, and it needs a tree on disk.
    pub fn with_workspace(mut self, root: impl Into<PathBuf>) -> Self {
        self.workspace = Some(root.into());
        self
    }

    /// What `bazel query '//external:*' --output=build` would have printed.
    pub fn with_external_rules(mut self, text: impl Into<String>) -> Self {
        self.external_rules = Some(text.into());
        self
    }

    /// Let `starlark_files` enumerate the whole in-memory tree, which is what
    /// the witness for a changed `.bzl` reads.
    pub fn enumerating_tree(mut self) -> Self {
        self.enumerates_tree = true;
        self
    }
}

impl PackageQuerier for Scanning {
    fn diagnostics(&self) -> Option<&cambium_engine::Diagnostics> {
        Some(&self.diags)
    }

    fn query(&self, packages: &[String]) -> Result<Rows, EngineError> {
        Ok(packages
            .iter()
            .map(|p| (self.rows)(p))
            .collect::<Vec<_>>()
            .into())
    }

    fn universe(&self) -> Result<Rows, EngineError> {
        self.universe_with(&[])
    }

    /// Delegated, not defaulted, and for the reason named on
    /// `PackageQuerier::universe_with`: the default answers `universe()`,
    /// which would drop the repositories and leave every test that widens the
    /// universe quietly testing the unwidened one.
    ///
    /// A package name of this querier that names an external repository
    /// (`@tsl//tsl/platform`) is returned only when that repository is one of
    /// `repos`, which is what a widened query does and what an unwidened one
    /// does not.
    fn universe_with(&self, repos: &[String]) -> Result<Rows, EngineError> {
        Ok(self
            .packages
            .iter()
            .filter(|p| match cambium_index::label::key_repo(p) {
                Some(r) => repos.iter().any(|x| x == r),
                None => true,
            })
            .map(|p| (self.rows)(p))
            .collect::<Vec<_>>()
            .into())
    }

    fn load_source(&self) -> Option<&dyn FileText> {
        self.files.as_ref().map(|f| f as &dyn FileText)
    }

    fn repo_mapping(&self) -> RepoMapping {
        self.repo_mapping
            .clone()
            .unwrap_or_else(RepoMapping::identity)
    }

    fn starlark_files(&self) -> Option<Vec<String>> {
        if self.enumerates_tree {
            self.files.as_ref().map(Files::paths)
        } else {
            None
        }
    }

    fn workspace(&self) -> Option<&Path> {
        self.workspace.as_deref()
    }

    fn external_rules(&self) -> Option<String> {
        self.external_rules.clone()
    }

    fn consumer_repos(
        &self,
        indexed: &[String],
    ) -> BTreeMap<String, BTreeMap<String, BTreeSet<String>>> {
        indexed
            .iter()
            .filter_map(|r| self.repo_consumers.get(r).map(|c| (r.clone(), c.clone())))
            .collect()
    }

    fn consumer_leaf_packages(
        &self,
        repo: &str,
        leaves: &[String],
    ) -> Option<BTreeMap<String, cambium_index::externals::LeafReach>> {
        self.leaf_questions
            .lock()
            .unwrap()
            .push((repo.to_string(), leaves.to_vec()));
        let known = self.consumer_leaves.get(repo)?;
        Some(
            leaves
                .iter()
                .filter_map(|l| known.get(l).map(|p| (l.clone(), p.clone())))
                .collect(),
        )
    }

    fn probe_repos(&self, repos: &[String]) -> Vec<cambium_bazel::RepoProbe> {
        self.probed.lock().unwrap().extend(repos.iter().cloned());
        repos
            .iter()
            .filter_map(|r| self.repo_probes.get(r).cloned())
            .collect()
    }

    /// A querier with a tree behind it can produce a loading layer, from the
    /// dump that tree would have made (see [`crate::sky`]). One without a
    /// tree cannot, and the index it builds carries no layer — which is the
    /// shape the tests of that very case want.
    fn can_dump_loading_layer(&self) -> bool {
        self.files.is_some()
    }

    fn dump_loading_layer(
        &self,
        filter: &str,
    ) -> Result<(String, cambium_index::sky::Paths), EngineError> {
        self.dumps.lock().unwrap().push(filter.to_string());
        let files = self.files.as_ref().ok_or_else(|| EngineError::BazelQuery {
            what: "dump --skyframe=deps".into(),
            code: None,
            stderr: "this querier has no tree".into(),
        })?;
        let packages: Vec<String> = self.packages.iter().map(|p| p.to_string()).collect();
        let tree = cambium_synth::sky::InMemory(&files.0);
        let mut text = cambium_synth::sky::dump(&packages, &tree);
        let nth = self
            .dumps_served
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let from = self
            .hides_from_dump
            .load(std::sync::atomic::Ordering::SeqCst);
        if self
            .hides_packages_until_reloaded
            .load(std::sync::atomic::Ordering::SeqCst)
            || (from > 0 && nth >= from)
        {
            text = without_package_blocks(&text);
        }
        Ok((text, cambium_synth::sky::paths()))
    }

    fn reload_named(&self, _packages: &[String]) -> bool {
        self.reloads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.hides_from_dump
            .store(0, std::sync::atomic::Ordering::SeqCst);
        true
    }

    fn reload_packages(&self, _repos: &[String]) -> bool {
        self.reloads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.hides_packages_until_reloaded
            .store(false, std::sync::atomic::Ordering::SeqCst);
        true
    }
}

/// A dump with every `PACKAGE` block dropped and everything else, including
/// `PACKAGE_LOOKUP`, left alone. A block is a line at column zero and the
/// indented lines under it.
fn without_package_blocks(text: &str) -> String {
    let mut out = String::new();
    let mut dropping = false;
    for line in text.lines() {
        let header = !line.starts_with(' ') && !line.is_empty();
        if header {
            dropping = line.starts_with("PACKAGE:");
        }
        if !dropping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// A querier that reads a toy BUILD syntax out of a directory: one rule per
/// line, `name dep dep ...`.
///
/// It stands in for Bazel wherever a test cares *which checkout* the rows came
/// from — it reads whatever directory it is pointed at, which is exactly the
/// substitution the revision-workspace bug was about — and wherever a test
/// needs the package set to follow real BUILD files appearing and disappearing
/// across commits (`revision_workspace`, `deleted_build_files`).
pub struct DirQuerier {
    pub root: PathBuf,
}

impl DirQuerier {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Every directory under the root holding a `BUILD` or `BUILD.bazel`,
    /// which is Bazel's own rule for what a package is.
    pub fn packages(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return out;
        };
        for e in entries.flatten() {
            if self.build_file(&e.file_name().to_string_lossy()).is_some() {
                out.push(e.file_name().to_string_lossy().into_owned());
            }
        }
        out.sort();
        out
    }

    /// The BUILD file Bazel would read for `pkg`. `BUILD` wins over
    /// `BUILD.bazel`, as it does in Bazel itself.
    fn build_file(&self, pkg: &str) -> Option<PathBuf> {
        let dir = self.root.join(pkg);
        [dir.join("BUILD"), dir.join("BUILD.bazel")]
            .into_iter()
            .find(|p| p.is_file())
    }

    pub fn update(&self, pkg: &str) -> PackageUpdate {
        let text = self
            .build_file(pkg)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let mut targets = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let mut words = line.split_whitespace();
            let name = words.next().unwrap();
            let inputs: Vec<String> = words.map(|d| d.to_string()).collect();
            targets.push(TargetRow {
                label: format!("//{pkg}:{name}"),
                kind: NodeKind::Rule,
                rule_class: Some("toy_library".into()),
                rule_hash: *blake3::hash(line.as_bytes()).as_bytes(),
                is_test: false,
                manual: false,
                inputs,
                generating_rule: None,
            });
        }
        PackageUpdate {
            package: pkg.to_string(),
            build_hash: *blake3::hash(text.as_bytes()).as_bytes(),
            bzl_hash: [0; 32],
            targets,
        }
    }
}

impl PackageQuerier for DirQuerier {
    fn query(&self, packages: &[String]) -> Result<Rows, EngineError> {
        Ok(packages
            .iter()
            .map(|p| self.update(p))
            .collect::<Vec<_>>()
            .into())
    }

    fn universe(&self) -> Result<Rows, EngineError> {
        Ok(self
            .packages()
            .iter()
            .map(|p| self.update(p))
            .collect::<Vec<_>>()
            .into())
    }

    fn workspace(&self) -> Option<&Path> {
        Some(&self.root)
    }

    fn can_dump_loading_layer(&self) -> bool {
        true
    }

    fn dump_loading_layer(
        &self,
        _filter: &str,
    ) -> Result<(String, cambium_index::sky::Paths), EngineError> {
        let tree = cambium_synth::sky::OnDisk(self.root.clone());
        Ok((
            cambium_synth::sky::dump(&self.packages(), &tree),
            cambium_synth::sky::paths(),
        ))
    }
}

// ---------------------------------------------------------------- published

/// A synthetic workspace with a real git history, a directory store, and an
/// index published for one of its commits — the fixture every test that
/// needs a warm-ish position shares (`cli_contract`, `bench_cli_contract`).
/// The rows come from the synthetic source, so Bazel never runs.
pub struct PublishedFixture {
    _dir: TempDir,
    /// The workspace: a git repository with `cambium.toml` pointing at the store.
    pub ws: PathBuf,
    /// The cache directory the index was published through.
    pub cache: PathBuf,
    /// The history's commits, oldest first.
    pub commits: Vec<String>,
    /// The published index's lineage, as hex.
    pub lineage: String,
}

/// Four commits of the `tiny` preset under `typical-pr` changes, with the
/// index for `commits[at]` published to the store.
pub fn published_fixture(at: usize) -> PublishedFixture {
    let dir = scratch();
    let store_dir = dir.path().join("store");
    let cache = dir.path().join("cache");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let graph = cambium_synth::generate(&cambium_synth::GraphSpec::preset("tiny").unwrap());
    let spec = cambium_synth::ChangeSpec {
        seed: 3,
        ..cambium_synth::ChangeSpec::preset("typical-pr").unwrap()
    };
    let emitted = cambium_synth::emit_git_history(graph, &spec, 4, &ws, true).expect("a history");
    let mut cfg = Config::load(&ws).unwrap();
    cfg.baseline.store = cambium_engine::config::StoreConfig::Dir {
        path: store_dir.clone(),
    };
    std::fs::write(ws.join("cambium.toml"), cfg.to_toml()).unwrap();

    let engine = Engine::with_parts(
        cfg.clone(),
        cambium_diff::Repo::new(ws.clone()),
        cambium_bazel::Bazel::at(ws.clone(), Some("/nonexistent/bazel")),
    )
    .with_cache_dir(cache.clone());
    let index = engine
        .full_build_with(
            &emitted.commits[at],
            &SynthQuerier::new(&emitted.graphs[at]),
        )
        .unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    let file = cache.join("published.cmbx");
    cambium_index::format::write_index(&file, &index).unwrap();
    let repo = cambium_diff::Repo::new(ws.clone());
    let tree = repo.tree_of(&emitted.commits[at]).unwrap();
    cambium_engine::BaselineCache::new(
        &cambium_engine::store::DirStore::new(store_dir),
        &cache,
        cfg.hash(),
    )
    .publish(&file, &index.header.lineage, &emitted.commits[at], &tree)
    .expect("publishing");

    PublishedFixture {
        _dir: dir,
        ws,
        cache,
        commits: emitted.commits,
        lineage: cambium_engine::probe::hex(&index.header.lineage),
    }
}
