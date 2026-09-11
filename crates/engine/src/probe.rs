//! Read-only probes: what `status`, `doctor` and `init` all want to know.
//!
//! Each probe answers one question about this machine and this checkout, and
//! returns a plain serde struct rather than printing: `status` renders them,
//! `doctor` judges them, `init` shows what it detected. Nothing here decides
//! whether a finding is acceptable — that is `doctor`'s whole job — and
//! nothing here prints.
//!
//! Two rules hold throughout. A probe never fails the caller: every one
//! returns its answer as an `Option` (absent because it could not be
//! determined) beside the diagnostics it raised, so one broken probe never
//! hides the others. And exactly one probe runs a process: [`bazel`], which
//! shells out for `bazel --version`. `status` does not call it — it reads
//! `.bazelversion` instead — because a cold Bazel server start is seconds and
//! `status` is meant to be instant.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::baseline::BaselineCache;
use crate::config::{Config, StoreConfig, CONFIG_FILE};
use crate::diag::{Code, Diagnostic, Diagnostics};
use crate::store::ArtifactStore;

/// The oldest Bazel `cambi` is tested against. Older than this still runs;
/// `doctor` says it is untested ([`Code::W017BazelVersionUnsupported`]).
pub const MIN_SUPPORTED_BAZEL_MAJOR: u32 = 6;

/// Prefix every probe object the store round-trip writes lives under, so it
/// is recognisable (and prunable) in a store a human looks at.
pub const PROBE_PREFIX: &str = ".probe/";

// ----------------------------------------------------------------- workspace

/// Which of the two dependency systems this workspace is using.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    /// `MODULE.bazel` decides the external repositories.
    Bzlmod,
    /// `WORKSPACE` does.
    Workspace,
    /// Both files are present and nothing on disk says which one Bazel will
    /// honour — there is no `.bazelversion` to read the default off.
    Ambiguous,
}

/// The workspace, as the filesystem describes it. No Bazel, no git.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceProbe {
    /// The workspace root, or absent when there is no marker file above the
    /// starting directory.
    pub root: Option<PathBuf>,
    pub mode: Option<WorkspaceMode>,
    /// The marker files actually present, in the order Bazel looks for them.
    pub markers: Vec<String>,
    /// A `MODULE.bazel` that sits beside a `WORKSPACE` Bazel will honour
    /// instead. It looks authoritative and is read by nothing: the shape that
    /// made a run's answer differ between two checkouts of one commit.
    pub inert_module_bazel: bool,
    /// The mirror image: a `WORKSPACE` that a bzlmod-era Bazel ignores.
    pub inert_workspace: bool,
    /// `.bazelversion`, verbatim and trimmed.
    pub bazelversion: Option<String>,
    /// The leading integer of `.bazelversion`, when it has one.
    pub bazelversion_major: Option<u32>,
    /// `try-import` targets from the root `.bazelrc`, as written.
    pub bazelrc_try_imports: Vec<String>,
    /// `import` targets from the root `.bazelrc`, as written.
    pub bazelrc_imports: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
}

const MARKER_FILES: &[&str] = &[
    "MODULE.bazel",
    "WORKSPACE",
    "WORKSPACE.bazel",
    "WORKSPACE.bzlmod",
];

/// Walks up from `start` for a workspace root and reads what is there.
pub fn workspace(start: &Path) -> WorkspaceProbe {
    let mut p = WorkspaceProbe {
        root: cambium_bazel::find_workspace_root(start),
        mode: None,
        markers: Vec::new(),
        inert_module_bazel: false,
        inert_workspace: false,
        bazelversion: None,
        bazelversion_major: None,
        bazelrc_try_imports: Vec::new(),
        bazelrc_imports: Vec::new(),
        diagnostics: Vec::new(),
    };
    let Some(root) = p.root.clone() else {
        return p;
    };
    p.markers = MARKER_FILES
        .iter()
        .filter(|m| root.join(m).exists())
        .map(|m| (*m).to_string())
        .collect();
    p.bazelversion = read_trimmed(&root.join(".bazelversion"));
    p.bazelversion_major = p.bazelversion.as_deref().and_then(leading_major);

    let module = p.markers.iter().any(|m| m == "MODULE.bazel");
    let workspace = p.markers.iter().any(|m| m != "MODULE.bazel");
    p.mode = match (module, workspace) {
        (true, false) => Some(WorkspaceMode::Bzlmod),
        (false, true) => Some(WorkspaceMode::Workspace),
        (false, false) => None,
        // Both. Which one Bazel reads is a function of its version — bzlmod is
        // the default from 7 on — so the pin decides, and without a pin
        // nothing on disk does.
        (true, true) => match p.bazelversion_major {
            Some(major) if major >= 7 => {
                p.inert_workspace = true;
                Some(WorkspaceMode::Bzlmod)
            }
            Some(_) => {
                p.inert_module_bazel = true;
                Some(WorkspaceMode::Workspace)
            }
            None => {
                p.diagnostics.push(
                    Diagnostic::warning(
                        Code::W016WorkspaceModeAmbiguous,
                        "MODULE.bazel and WORKSPACE are both present and there is no \
                         .bazelversion, so which one Bazel honours depends on whichever \
                         Bazel runs",
                    )
                    .at("MODULE.bazel", None)
                    .with_help(
                        "pin the Bazel version in .bazelversion, or delete the file the \
                         build does not use: a run that reads the other one indexes a \
                         different graph",
                    ),
                );
                Some(WorkspaceMode::Ambiguous)
            }
        },
    };

    let rc = root.join(".bazelrc");
    if let Ok(text) = std::fs::read_to_string(&rc) {
        for line in crate::bazelrc::parse(&text).imports {
            match line.strip_prefix("try-import ") {
                Some(target) => p.bazelrc_try_imports.push(target.to_string()),
                None => {
                    if let Some(target) = line.strip_prefix("import ") {
                        p.bazelrc_imports.push(target.to_string());
                    }
                }
            }
        }
    }
    p
}

// --------------------------------------------------------------------- bazel

/// The Bazel binary a run would use, and what it says its version is.
///
/// **The only probe that runs a process.** `status` must not call it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BazelProbe {
    /// The binary as configured or discovered, before any PATH lookup.
    pub binary: String,
    /// Where it actually is, when it could be found.
    pub resolved: Option<PathBuf>,
    /// `bazel --version`, verbatim (`bazel 7.4.1`).
    pub version: Option<String>,
    pub major: Option<u32>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Resolves the Bazel binary and asks it for its version.
pub fn bazel(root: &Path, cfg: &Config, binary_override: Option<&str>) -> BazelProbe {
    let configured = binary_override
        .filter(|b| !b.is_empty())
        .or(Some(cfg.bazel.binary.as_str()).filter(|b| !b.is_empty()));
    let bazel = cambium_bazel::Bazel::at(root.to_path_buf(), configured);
    let binary = bazel.binary().to_path_buf();
    let mut p = BazelProbe {
        binary: binary.display().to_string(),
        resolved: resolve_program(&binary),
        version: None,
        major: None,
        diagnostics: Vec::new(),
    };
    if p.resolved.is_none() {
        p.diagnostics.push(
            Diagnostic::error(
                Code::E015BazelNotFound,
                format!("the Bazel binary `{}` is not on PATH", p.binary),
            )
            .with_help(
                "install Bazel or bazelisk, or name the binary with --bazel-binary, \
                 [bazel].binary in cambium.toml, or bazel.binary in cambium.local.toml",
            ),
        );
        return p;
    }
    let raw = bazel.version();
    if raw.is_empty() {
        p.diagnostics.push(
            Diagnostic::error(
                Code::E015BazelNotFound,
                format!("`{} --version` produced nothing", p.binary),
            )
            .with_help("run it by hand: a bazelisk that cannot download a release says why"),
        );
        return p;
    }
    p.major = leading_major(raw.trim_start_matches("bazel").trim());
    p.version = Some(raw);
    if let Some(major) = p.major {
        if major < MIN_SUPPORTED_BAZEL_MAJOR {
            p.diagnostics.push(
                Diagnostic::warning(
                    Code::W017BazelVersionUnsupported,
                    format!(
                        "Bazel {major} is older than {MIN_SUPPORTED_BAZEL_MAJOR}, which is the \
                         oldest version cambium is tested against"
                    ),
                )
                .with_help(
                    "upgrade Bazel, or pin a tested version in .bazelversion; an untested \
                     version is not refused, only unverified",
                ),
            );
        }
    }
    p
}

// -------------------------------------------------------------------- config

/// Where the configuration came from and whether it is usable.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigProbe {
    /// The file a run would read, whether or not it exists.
    pub path: PathBuf,
    pub exists: bool,
    /// Absent when the file is there and does not parse.
    pub hash: Option<String>,
    pub parse_error: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Reads `cambium.toml` and says what it found. Never fails: an unparsable
/// file is a diagnostic, because `status` still has everything else to print.
pub fn config(root: &Path, config_path: Option<&Path>) -> (Option<Config>, ConfigProbe) {
    let path = config_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join(CONFIG_FILE));
    let mut p = ConfigProbe {
        exists: path.exists(),
        path: path.clone(),
        hash: None,
        parse_error: None,
        diagnostics: Vec::new(),
    };
    match Config::load_from(&path) {
        Ok(cfg) => {
            p.hash = Some(hex(&cfg.hash()));
            p.diagnostics = cfg.removed_key_diagnostics();
            (Some(cfg), p)
        }
        Err(e) => {
            p.parse_error = Some(e.to_string());
            p.diagnostics.push(
                Diagnostic::error(Code::E002Config, e.to_string())
                    .at(path.display().to_string(), None)
                    .with_help("fix the file, or point --config at another one"),
            );
            (None, p)
        }
    }
}

// --------------------------------------------------------------------- store

/// The store a run would read, and whether it answers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreProbe {
    /// `none`, `dir`, `http` or `command`.
    pub kind: String,
    /// Where it is, as the backend describes itself.
    pub location: Option<String>,
    pub configured: bool,
    /// Whether the backend can enumerate; a store that cannot is probed with
    /// a fetch instead.
    pub listable: bool,
    /// Absent when the store is not configured — there is nothing to reach.
    pub reachable: Option<bool>,
    /// Index blobs held, when the backend can be listed.
    pub indexes: Option<usize>,
    /// Lineage ids the listing shows, newest-first order not implied.
    pub lineages: Vec<String>,
    pub error: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Reachability, by the cheapest question the backend answers: a listing when
/// it has one, otherwise a fetch of a name that cannot exist — a miss is a
/// reachable store, an error is not.
pub fn store(cfg: &Config, store: Option<&dyn ArtifactStore>) -> StoreProbe {
    let mut p = StoreProbe {
        kind: store_kind(&cfg.baseline.store).to_string(),
        location: store.map(|s| s.describe()),
        configured: store.is_some(),
        listable: false,
        reachable: None,
        indexes: None,
        lineages: Vec::new(),
        error: None,
        diagnostics: Vec::new(),
    };
    let Some(s) = store else {
        return p;
    };
    match s.list() {
        Ok(names) => {
            p.listable = true;
            p.reachable = Some(true);
            p.indexes = Some(names.iter().filter(|n| n.ends_with(".bin")).count());
            let mut lineages: Vec<String> = names.iter().filter_map(|n| lineage_of(n)).collect();
            lineages.sort();
            lineages.dedup();
            p.lineages = lineages;
        }
        Err(crate::store::StoreError::Unsupported(_)) => {
            // A fetch of a name no publisher writes: reachable iff the
            // backend can tell us it is absent.
            let dest = std::env::temp_dir().join(format!(
                "cambium-probe.{}",
                cambium_index::format::temp_suffix()
            ));
            match s.fetch(&absent_probe_name(), &dest) {
                Ok(_) => p.reachable = Some(true),
                Err(e) => {
                    p.reachable = Some(false);
                    p.error = Some(e.to_string());
                    p.diagnostics.push(unreachable_diagnostic(&e.to_string()));
                }
            }
            let _ = std::fs::remove_file(&dest);
        }
        Err(e) => {
            p.reachable = Some(false);
            p.error = Some(e.to_string());
            p.diagnostics.push(unreachable_diagnostic(&e.to_string()));
        }
    }
    p
}

fn unreachable_diagnostic(detail: &str) -> Diagnostic {
    Diagnostic::warning(
        Code::W007StoreFetchFailed,
        format!("the index store could not be reached: {detail}"),
    )
    .with_help(
        "check the path, URL or credentials in [baseline].store; until it answers, every \
         run builds its own index from scratch",
    )
}

/// The name the reachability probe asks for. Not a key any publisher writes,
/// so a store that answers "absent" is a store that is answering.
fn absent_probe_name() -> String {
    format!("{PROBE_PREFIX}absent-by-construction")
}

/// A publish and a fetch-back of a small object — `doctor` only, because it
/// writes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoundTripProbe {
    /// The key written, left in place: no backend here can delete one.
    pub name: String,
    pub published: bool,
    pub fetched: bool,
    /// Whether what came back is what went out.
    pub identical: bool,
    pub error: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Writes a probe object, reads it back, and compares the bytes.
pub fn store_round_trip(store: &dyn ArtifactStore, cache_dir: &Path) -> RoundTripProbe {
    let name = format!(
        "{PROBE_PREFIX}round-trip.{}",
        cambium_index::format::temp_suffix()
    );
    let mut p = RoundTripProbe {
        name: name.clone(),
        published: false,
        fetched: false,
        identical: false,
        error: None,
        diagnostics: Vec::new(),
    };
    let body = format!("cambium store probe {name}\n");
    let src = cache_dir.join("probe.out");
    let dest = cache_dir.join("probe.in");
    let fail = |p: &mut RoundTripProbe, what: &str, detail: String| {
        p.error = Some(detail.clone());
        p.diagnostics.push(
            Diagnostic::error(
                Code::E016StoreRoundTripFailed,
                format!("the index store could not {what}: {detail}"),
            )
            .with_help(
                "a run publishes indexes through the same calls; check that the store in \
                 [baseline].store is writable with the credentials in this environment",
            ),
        );
    };
    if let Err(e) = std::fs::create_dir_all(cache_dir).and_then(|()| std::fs::write(&src, &body)) {
        fail(&mut p, "be probed from the cache directory", e.to_string());
        return p;
    }
    if let Err(e) = store.publish(&name, &src) {
        fail(&mut p, "be written to", e.to_string());
        let _ = std::fs::remove_file(&src);
        return p;
    }
    p.published = true;
    match store.fetch(&name, &dest) {
        Ok(true) => {
            p.fetched = true;
            p.identical = std::fs::read_to_string(&dest).is_ok_and(|got| got == body);
            if !p.identical {
                fail(
                    &mut p,
                    "return the object it was given",
                    "the bytes fetched back differ from the bytes published".into(),
                );
            }
        }
        Ok(false) => fail(
            &mut p,
            "find the object it had just accepted",
            "the fetch reported it absent".into(),
        ),
        Err(e) => fail(&mut p, "be read back from", e.to_string()),
    }
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dest);
    p
}

// ------------------------------------------------------------------ position

/// What the next run would have to do to answer.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Plan {
    /// An index for HEAD itself is in the store.
    Warm,
    /// An ancestor's index is, and the run brings it forward.
    Splice,
    /// Nothing usable in the store: the run runs `bazel query //...`.
    Cold,
}

impl std::fmt::Display for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Plan::Warm => "warm",
            Plan::Splice => "splice",
            Plan::Cold => "cold",
        })
    }
}

/// Where HEAD sits relative to the indexes the store holds: the line that
/// answers "warm or cold?".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PositionProbe {
    pub head: Option<String>,
    /// The tree HEAD points at — what indexes are actually keyed by, so that a
    /// rebase of an indexed commit is still warm.
    pub head_tree: Option<String>,
    pub dirty: bool,
    /// An index for HEAD itself is in the store.
    pub has_index: bool,
    /// The nearest first-parent ancestor that has one, when HEAD does not.
    pub nearest: Option<String>,
    /// How many commits back `nearest` is.
    pub distance: Option<usize>,
    /// How far the walk was allowed to look (`baseline.max_ancestor_walk`).
    pub max_ancestor_walk: usize,
    /// What the next run would do: `warm`, `splice` or `cold`.
    pub plan: Plan,
    /// The index file the fetch left in the cache, for [`index_at`].
    pub index_path: Option<PathBuf>,
    /// Lineages the walk found and refused because they were built with
    /// another `cambium.toml`, rule-hash version or index content revision;
    /// each has a `CAMBI-W003` in `diagnostics`. What [`lineage`] reports as
    /// conflicting.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incompatible_lineages: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Asks the store for HEAD's index, then walks first-parent ancestors for one.
///
/// The same walk `Engine::resolve_baseline` does, with the same compatibility
/// rule: an index built with another `cambium.toml` is not a baseline, it is a
/// leftover, and the walk steps past it.
pub fn position(
    repo: &cambium_diff::Repo,
    store: Option<&dyn ArtifactStore>,
    cache_dir: &Path,
    config_hash: [u8; 32],
    max_ancestor_walk: usize,
) -> PositionProbe {
    position_at(
        repo,
        "HEAD",
        store,
        cache_dir,
        config_hash,
        max_ancestor_walk,
    )
}

/// [`position`] about a revision that is not HEAD.
///
/// `cambi affected BASE..HEAD` loads an index for *BASE*, so the plan
/// `affected --dry-run` prints is about that revision and not about where the
/// working tree happens to be.
pub fn position_at(
    repo: &cambium_diff::Repo,
    rev: &str,
    store: Option<&dyn ArtifactStore>,
    cache_dir: &Path,
    config_hash: [u8; 32],
    max_ancestor_walk: usize,
) -> PositionProbe {
    let mut p = PositionProbe {
        head: repo.rev_parse(rev).ok(),
        head_tree: repo.tree_of(rev).ok(),
        dirty: repo.is_dirty(),
        has_index: false,
        nearest: None,
        distance: None,
        max_ancestor_walk,
        plan: Plan::Cold,
        index_path: None,
        incompatible_lineages: Vec::new(),
        diagnostics: Vec::new(),
    };
    let (Some(store), Some(head)) = (store, p.head.clone()) else {
        return p;
    };
    let diags = Diagnostics::new();
    let cache = BaselineCache::new(store, cache_dir, config_hash).with_diagnostics(diags.clone());
    let tree = p.head_tree.clone().unwrap_or_else(|| head.clone());
    match cache.fetch(&head, &tree) {
        Ok(Some(path)) => {
            p.has_index = true;
            p.nearest = Some(head.clone());
            p.distance = Some(0);
            p.plan = Plan::Warm;
            p.index_path = Some(path);
        }
        Ok(None) => {
            let ancestors = repo
                .first_parent_ancestors(&head, max_ancestor_walk)
                .unwrap_or_default();
            for (back, anc) in ancestors.iter().enumerate() {
                let anc_tree = repo.tree_of(anc).unwrap_or_else(|_| anc.clone());
                if let Ok(Some(path)) = cache.fetch(anc, &anc_tree) {
                    p.nearest = Some(anc.clone());
                    // `first_parent_ancestors` starts at HEAD's parent, so the
                    // first entry is one commit back.
                    p.distance = Some(back + 1);
                    p.plan = Plan::Splice;
                    p.index_path = Some(path);
                    break;
                }
            }
        }
        Err(e) => {
            p.diagnostics.push(unreachable_diagnostic(&e.to_string()));
        }
    }
    // The walk can observe one fact twice — a tree key found both directly and
    // through its `commits/` pointer is fetched, and refused, twice — and a
    // reader of `status` should see each finding once.
    diags.drain_into(&mut p.diagnostics);
    p.incompatible_lineages = cache.refused_lineages();
    p
}

// --------------------------------------------------------------------- index

/// One index file's header: what a run would be splicing onto.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexProbe {
    pub path: PathBuf,
    pub size_bytes: u64,
    /// Bytes after decompression, when the file on disk is a zstd frame.
    pub framed_bytes: Option<u64>,
    pub format_version: u32,
    pub commit: String,
    pub lineage: String,
    pub config_hash: String,
    /// The Bazel that answered the queries this index was built from. The
    /// index records no `cambi` version of its own; what identifies the engine
    /// that wrote it is `format_version` plus `config_hash`, which mixes in
    /// the rule-hash version and the index content revision.
    pub bazel_version: String,
    pub built_at_unix: u64,
    pub packages: u32,
    /// Rule targets. `nodes` counts files and packages too.
    pub targets: u32,
    pub nodes: u32,
    pub edges: u64,
    pub external_repos: u32,
    pub partial: bool,
    pub diagnostics: Vec<Diagnostic>,
}

/// Reads one index's header, or says why it could not.
pub fn index_at(path: &Path) -> (Option<IndexProbe>, Vec<Diagnostic>) {
    let f = match cambium_index::IndexFile::open(path) {
        Ok(f) => f,
        Err(cambium_index::IndexError::BadVersion {
            found, expected, ..
        }) => {
            return (
                None,
                vec![Diagnostic::warning(
                    Code::W018IndexFormatOlder,
                    format!(
                        "the index at {} is format version {found} and this cambi reads \
                         {expected}, so it cannot be spliced onto",
                        path.display()
                    ),
                )
                .with_help(
                    "mint a fresh lineage with `cambi index build --publish`; the old \
                     blobs age out under the retention policy",
                )],
            );
        }
        Err(e) => {
            return (
                None,
                vec![Diagnostic::error(
                    Code::E001Internal,
                    format!("the index at {} could not be read: {e}", path.display()),
                )],
            );
        }
    };
    let v = f.view();
    let q = crate::inspect::IndexQuery::new(v);
    let mut p = IndexProbe {
        path: path.to_path_buf(),
        size_bytes: f.file_size() as u64,
        framed_bytes: f.is_compressed().then(|| f.framed_size() as u64),
        format_version: v.format_version,
        commit: v.commit.to_string(),
        lineage: hex(&v.lineage),
        config_hash: hex(&v.config_hash),
        bazel_version: v.bazel_version.to_string(),
        built_at_unix: v.created_unix,
        packages: v.num_packages(),
        targets: q.kind_histogram().get("rule").copied().unwrap_or(0),
        nodes: v.num_nodes(),
        edges: v.num_edges() as u64,
        external_repos: v.num_repos(),
        partial: v.partial,
        diagnostics: Vec::new(),
    };
    if let Some(w) = crate::inspect::PartialLoad::from(v).warning() {
        p.diagnostics
            .push(Diagnostic::warning(Code::W006PartialQuery, w).with_help(
                "fix the load errors and rebuild, or accept the partial index deliberately \
                 with baseline.publish_partial",
            ));
    }
    let diagnostics = p.diagnostics.clone();
    (Some(p), diagnostics)
}

// ------------------------------------------------------------------- lineage

/// The lineage a run would splice onto, and what else the store holds.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LineageProbe {
    /// The lineage of the index a run would load.
    pub current: Option<String>,
    /// Every other lineage the listing shows.
    pub others: Vec<String>,
    /// Lineages a fetch proved incompatible: their `config_hash` is not this
    /// workspace's, or they were written by an engine with a different
    /// rule-hash version or index content revision. The position walk is the
    /// only place that difference is actually observed, and it records them
    /// beside the `CAMBI-W003` it raises for each.
    pub conflicting: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Cross-references the store listing against the index a run would load.
pub fn lineage(
    store: &StoreProbe,
    position: &PositionProbe,
    index: Option<&IndexProbe>,
) -> LineageProbe {
    let current = index.map(|i| i.lineage.clone());
    let mut others: Vec<String> = store
        .lineages
        .iter()
        .filter(|l| Some(*l) != current.as_ref())
        .cloned()
        .collect();
    others.sort();
    others.dedup();
    let mut conflicting = position.incompatible_lineages.clone();
    conflicting.sort();
    conflicting.dedup();
    let mut p = LineageProbe {
        current,
        others,
        conflicting,
        diagnostics: Vec::new(),
    };
    if !p.conflicting.is_empty() {
        p.diagnostics.push(
            Diagnostic::warning(
                Code::W019LineageConflict,
                format!(
                    "the store holds {} lineage(s) built with a different cambium.toml, \
                     rule-hash version or index content revision: {}",
                    p.conflicting.len(),
                    p.conflicting.join(", ")
                ),
            )
            .with_help(
                "expected right after a cambium.toml edit or a cambi upgrade; the stale \
                 lineage is skipped, not used, and ages out under [baseline].retention_days. \
                 Run `cambi index gc` to remove it now",
            ),
        );
    }
    p
}

// ------------------------------------------------------------------- the lot

/// What to probe. The two process- and write-bearing probes are opt-in.
#[derive(Clone, Debug)]
pub struct Options {
    /// Where to start looking for a workspace.
    pub start: PathBuf,
    pub config_path: Option<PathBuf>,
    pub bazel_binary: Option<String>,
    pub cache_dir: Option<PathBuf>,
    /// Run `bazel --version`. `status` does not.
    pub run_bazel: bool,
    /// Publish and fetch back a probe object. `doctor` only.
    pub store_round_trip: bool,
}

impl Options {
    pub fn new(start: impl Into<PathBuf>) -> Self {
        Self {
            start: start.into(),
            config_path: None,
            bazel_binary: None,
            cache_dir: None,
            run_bazel: false,
            store_round_trip: false,
        }
    }
}

/// Every probe's answer, each one independent of the rest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Probes {
    pub workspace: WorkspaceProbe,
    pub config: ConfigProbe,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bazel: Option<BazelProbe>,
    pub store: StoreProbe,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub round_trip: Option<RoundTripProbe>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<PositionProbe>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<IndexProbe>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lineage: Option<LineageProbe>,
}

impl Probes {
    /// Every diagnostic every probe raised, in the order the sections print
    /// and each one once: an index header the reader refused is filed with
    /// the position it was found for as well as with the index itself.
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        let sections: [Option<&[Diagnostic]>; 8] = [
            Some(&self.workspace.diagnostics),
            Some(&self.config.diagnostics),
            self.bazel.as_ref().map(|b| b.diagnostics.as_slice()),
            Some(&self.store.diagnostics),
            self.round_trip.as_ref().map(|r| r.diagnostics.as_slice()),
            self.position.as_ref().map(|p| p.diagnostics.as_slice()),
            self.index.as_ref().map(|i| i.diagnostics.as_slice()),
            self.lineage.as_ref().map(|l| l.diagnostics.as_slice()),
        ];
        let mut out = Vec::new();
        for s in sections.into_iter().flatten() {
            crate::diag::extend_unique(&mut out, s.iter().cloned());
        }
        out
    }

    /// The effective configuration, when there was one to read.
    pub fn config_ok(&self) -> bool {
        self.config.parse_error.is_none()
    }
}

/// Runs the probes `opts` asks for. Never fails.
pub fn collect(opts: &Options) -> Probes {
    let ws = workspace(&opts.start);
    let root = ws.root.clone().unwrap_or_else(|| opts.start.clone());
    let (cfg, config_probe) = config(&root, opts.config_path.as_deref());
    let effective = cfg.clone().unwrap_or_default();

    let bazel_probe = opts
        .run_bazel
        .then(|| bazel(&root, &effective, opts.bazel_binary.as_deref()));

    // The store's own diagnostics (a `command` backend's failed fetch) go into
    // the store probe, not into a sink nobody drains.
    let sink = Diagnostics::new();
    let backend = crate::store::from_config(&effective.baseline.store, &sink);
    let mut store_probe = store(&effective, backend.as_deref());
    store_probe.diagnostics.extend(sink.drain());

    let cache_dir = opts
        .cache_dir
        .clone()
        .or_else(|| effective.baseline.cache_dir.clone())
        .unwrap_or_else(crate::engine::default_cache_dir);

    let round_trip = match (opts.store_round_trip, backend.as_deref()) {
        (true, Some(s)) => Some(store_round_trip(s, &cache_dir)),
        _ => None,
    };

    // From the workspace root when there is one, and from where the caller
    // started when there is not: a git checkout that is not (yet) a Bazel
    // workspace still has a HEAD, and "no index for it" is a more useful
    // answer than silence.
    let position_probe = {
        let root = ws.root.clone().unwrap_or_else(|| opts.start.clone());
        cambium_diff::Repo::discover(&root).ok().map(|repo| {
            position(
                &repo,
                backend.as_deref(),
                &cache_dir,
                effective.hash(),
                effective.baseline.max_ancestor_walk,
            )
        })
    };

    let (index_probe, index_diags) =
        match position_probe.as_ref().and_then(|p| p.index_path.clone()) {
            Some(path) => index_at(&path),
            None => (None, Vec::new()),
        };
    let mut position_probe = position_probe;
    if let (Some(pos), false) = (position_probe.as_mut(), index_diags.is_empty()) {
        // A header the reader refuses belongs with the position it was found
        // for, so `status` prints it in Warnings either way.
        pos.diagnostics.extend(
            index_diags
                .into_iter()
                .filter(|d| d.code == Code::W018IndexFormatOlder),
        );
    }

    let lineage_probe = position_probe
        .as_ref()
        .map(|pos| lineage(&store_probe, pos, index_probe.as_ref()));

    Probes {
        workspace: ws,
        config: config_probe,
        bazel: bazel_probe,
        store: store_probe,
        round_trip,
        position: position_probe,
        index: index_probe,
        lineage: lineage_probe,
    }
}

// ------------------------------------------------------------------- helpers

fn read_trimmed(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// The leading integer of a version string: `7.4.1` is 7, `8.0.0rc2` is 8.
fn leading_major(s: &str) -> Option<u32> {
    let digits: String = s.trim().chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Where a program name actually resolves. A name with a separator in it is
/// taken as a path; a bare name is looked up on `$PATH`, the way the shell
/// would.
fn resolve_program(bin: &Path) -> Option<PathBuf> {
    if bin.components().count() > 1 {
        return bin.is_file().then(|| bin.to_path_buf());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(bin))
        .find(|c| c.is_file())
}

fn store_kind(cfg: &StoreConfig) -> &'static str {
    match cfg {
        StoreConfig::None => "none",
        StoreConfig::Dir { .. } => "dir",
        StoreConfig::Http { .. } => "http",
        StoreConfig::Command { .. } => "command",
    }
}

/// The lineage segment of `indexes/<lineage>/<tree>.bin`. The lineage-less
/// fallback keys (`indexes/by-tree/...`) name no lineage and yield nothing.
fn lineage_of(key: &str) -> Option<String> {
    let rest = key.strip_prefix("indexes/")?;
    let (lineage, _) = rest.split_once('/')?;
    let ok = lineage.len() == 32 && lineage.chars().all(|c| c.is_ascii_hexdigit());
    ok.then(|| lineage.to_string())
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The first seven hex characters, the way git abbreviates a sha.
pub fn short(rev: &str) -> String {
    rev.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn a_module_only_tree_is_bzlmod_and_a_workspace_only_tree_is_not() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "MODULE.bazel", "module(name = \"x\")\n");
        let p = workspace(d.path());
        assert_eq!(p.mode, Some(WorkspaceMode::Bzlmod));
        assert!(!p.inert_module_bazel && !p.inert_workspace);
        assert!(p.diagnostics.is_empty());

        let d = tempfile::tempdir().unwrap();
        write(d.path(), "WORKSPACE", "workspace(name = \"x\")\n");
        assert_eq!(workspace(d.path()).mode, Some(WorkspaceMode::Workspace));
    }

    /// The shape that made one commit index two different graphs: a
    /// `MODULE.bazel` nothing reads, beside a `WORKSPACE` that is read.
    #[test]
    fn the_pin_decides_which_of_the_two_files_is_inert() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "MODULE.bazel", "module(name = \"x\")\n");
        write(d.path(), "WORKSPACE", "");
        write(d.path(), ".bazelversion", "6.5.0\n");
        let p = workspace(d.path());
        assert_eq!(p.mode, Some(WorkspaceMode::Workspace));
        assert!(p.inert_module_bazel, "bazel 6 reads WORKSPACE");
        assert!(p.diagnostics.is_empty(), "the pin answered it");

        std::fs::write(d.path().join(".bazelversion"), "7.4.1\n").unwrap();
        let p = workspace(d.path());
        assert_eq!(p.mode, Some(WorkspaceMode::Bzlmod));
        assert!(p.inert_workspace, "bazel 7 reads MODULE.bazel");
        assert_eq!(p.bazelversion.as_deref(), Some("7.4.1"));
        assert_eq!(p.bazelversion_major, Some(7));
    }

    #[test]
    fn both_files_and_no_pin_is_ambiguous_and_says_so() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "MODULE.bazel", "");
        write(d.path(), "WORKSPACE", "");
        let p = workspace(d.path());
        assert_eq!(p.mode, Some(WorkspaceMode::Ambiguous));
        assert_eq!(p.diagnostics.len(), 1);
        assert_eq!(p.diagnostics[0].code, Code::W016WorkspaceModeAmbiguous);
        assert!(p.diagnostics[0].help.is_some());
    }

    #[test]
    fn outside_a_workspace_every_field_is_simply_absent() {
        let d = tempfile::tempdir().unwrap();
        let p = workspace(&d.path().join("nowhere"));
        assert!(p.root.is_none());
        assert!(p.mode.is_none());
        assert!(p.markers.is_empty());
        assert!(p.diagnostics.is_empty(), "absence is not a diagnostic here");
    }

    #[test]
    fn the_bazelrc_imports_are_split_by_keyword() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "MODULE.bazel", "");
        write(
            d.path(),
            ".bazelrc",
            "build --verbose_failures\nimport %workspace%/shared.bazelrc\n\
             try-import %workspace%/user.bazelrc\n",
        );
        let p = workspace(d.path());
        assert_eq!(p.bazelrc_try_imports, vec!["%workspace%/user.bazelrc"]);
        assert_eq!(p.bazelrc_imports, vec!["%workspace%/shared.bazelrc"]);
    }

    #[test]
    fn an_unparsable_config_is_a_diagnostic_not_a_failure() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), CONFIG_FILE, "[bazel\n");
        let (cfg, p) = config(d.path(), None);
        assert!(cfg.is_none());
        assert!(p.exists && p.hash.is_none());
        assert_eq!(p.diagnostics[0].code, Code::E002Config);
    }

    #[test]
    fn a_missing_config_is_the_defaults_and_a_hash() {
        let d = tempfile::tempdir().unwrap();
        let (cfg, p) = config(d.path(), None);
        assert_eq!(cfg, Some(Config::default()));
        assert!(!p.exists);
        assert_eq!(p.hash, Some(hex(&Config::default().hash())));
    }

    #[test]
    fn an_unconfigured_store_is_not_unreachable_it_is_absent() {
        let p = store(&Config::default(), None);
        assert_eq!(p.kind, "none");
        assert!(!p.configured);
        assert_eq!(p.reachable, None, "nothing to reach is not a failure");
        assert!(p.diagnostics.is_empty());
    }

    #[test]
    fn a_directory_store_lists_and_its_lineages_are_read_off_the_keys() {
        let d = tempfile::tempdir().unwrap();
        let backend = crate::store::DirStore::new(d.path().to_path_buf());
        let blob = d.path().join("blob");
        std::fs::write(&blob, "x").unwrap();
        let lineage = "0".repeat(32);
        backend
            .publish(&format!("indexes/{lineage}/abc.bin"), &blob)
            .unwrap();
        let mut cfg = Config::default();
        cfg.baseline.store = StoreConfig::Dir {
            path: d.path().to_path_buf(),
        };
        let p = store(&cfg, Some(&backend));
        assert_eq!(p.kind, "dir");
        assert_eq!(p.reachable, Some(true));
        assert!(p.listable);
        assert_eq!(p.indexes, Some(1));
        assert_eq!(p.lineages, vec![lineage]);
    }

    #[test]
    fn the_round_trip_publishes_under_the_probe_prefix_and_reads_back() {
        let d = tempfile::tempdir().unwrap();
        let backend = crate::store::DirStore::new(d.path().join("store"));
        let p = store_round_trip(&backend, &d.path().join("cache"));
        assert!(p.name.starts_with(PROBE_PREFIX), "{}", p.name);
        assert!(p.published && p.fetched && p.identical, "{p:?}");
        assert!(p.diagnostics.is_empty());
    }

    #[test]
    fn a_store_that_cannot_be_written_fails_the_round_trip_with_a_code() {
        let d = tempfile::tempdir().unwrap();
        // A file where the store's root directory should be: every publish
        // into it fails.
        let root = d.path().join("not-a-dir");
        std::fs::write(&root, "").unwrap();
        let backend = crate::store::DirStore::new(root);
        let p = store_round_trip(&backend, &d.path().join("cache"));
        assert!(!p.published);
        assert_eq!(p.diagnostics[0].code, Code::E016StoreRoundTripFailed);
        assert!(p.diagnostics[0].help.is_some());
    }

    #[test]
    fn a_lineage_is_read_off_a_store_key_and_nothing_else_is() {
        let l = "a".repeat(32);
        assert_eq!(lineage_of(&format!("indexes/{l}/ff.bin")), Some(l.clone()));
        assert_eq!(lineage_of("indexes/by-tree/ff.bin"), None);
        assert_eq!(lineage_of("commits/by-commit/ff"), None);
    }

    #[test]
    fn a_mismatched_lineage_in_the_store_is_named_once_with_a_code() {
        let l = "b".repeat(32);
        let pos = PositionProbe {
            head: Some("deadbeef".into()),
            head_tree: None,
            dirty: false,
            has_index: false,
            nearest: None,
            distance: None,
            max_ancestor_walk: 20,
            plan: Plan::Cold,
            index_path: None,
            incompatible_lineages: vec![l.clone(), l.clone()],
            diagnostics: vec![Diagnostic::warning(
                Code::W003IndexConfigMismatch,
                format!("skipping the stored index `indexes/{l}/ff.bin`: it was built with ..."),
            )],
        };
        let mut sp = store(&Config::default(), None);
        sp.lineages = vec![l.clone()];
        let p = lineage(&sp, &pos, None);
        assert_eq!(p.conflicting, vec![l.clone()]);
        assert_eq!(p.others, vec![l]);
        assert_eq!(p.diagnostics[0].code, Code::W019LineageConflict);
    }

    #[test]
    fn a_version_string_yields_its_major_however_it_is_spelled() {
        assert_eq!(leading_major("7.4.1"), Some(7));
        assert_eq!(leading_major(" 8.0.0rc2 "), Some(8));
        assert_eq!(leading_major("last_green"), None);
    }
}
