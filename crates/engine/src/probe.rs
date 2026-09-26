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
use crate::config::{Config, Overrides, StoreConfig, CONFIG_FILE};
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

/// Whether a `.bazeliskrc` at `root` names the Bazel version, which pins
/// it as `.bazelversion` would.
fn bazeliskrc_pins(root: &Path) -> bool {
    std::fs::read_to_string(root.join(".bazeliskrc")).is_ok_and(|t| {
        t.lines()
            .any(|l| l.trim_start().starts_with("USE_BAZEL_VERSION="))
    })
}

/// Whether the file at `path` declares nothing: every line blank or a
/// comment. The `MODULE.bazel` Bazel 7 and later write on their first run in
/// a tree without one is such a file, and any run leaves it behind, cambium's
/// own cold query included.
fn declares_nothing(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|t| {
        t.lines().all(|l| {
            let l = l.trim();
            l.is_empty() || l.starts_with('#')
        })
    })
}

/// Whether the tree's `MODULE.bazel` counts as a marker. Beside a `WORKSPACE`
/// file, one that declares nothing does not: the dependencies are all in the
/// `WORKSPACE`, and the stub Bazel writes would otherwise make every such
/// tree read as having both files once any Bazel had run in it.
fn module_counts(root: &Path, markers: &[String]) -> bool {
    markers.iter().any(|m| m == "MODULE.bazel")
        && !(markers.iter().any(|m| m != "MODULE.bazel")
            && declares_nothing(&root.join("MODULE.bazel")))
}

/// Whether the root `.bazelrc` leaves `--enable_bzlmod` on, the last setting
/// of it winning as it does in Bazel. `--noenable_bzlmod` and
/// `--enable_bzlmod=false` turn it off.
fn bazelrc_enables_bzlmod(root: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(root.join(".bazelrc")) else {
        return false;
    };
    let mut on = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default();
        for tok in line.split_whitespace().skip(1) {
            match tok {
                "--enable_bzlmod" => on = true,
                "--noenable_bzlmod" => on = false,
                _ => {
                    if let Some(v) = tok.strip_prefix("--enable_bzlmod=") {
                        on = matches!(v, "true" | "1" | "yes");
                    }
                }
            }
        }
    }
    on
}

/// `CAMBI-E015` when the Bazel that answered `--version` cannot read the
/// workspace in the mode its marker files leave it: a tree with only a
/// `WORKSPACE` file under Bazel 9, which reads none; or a tree with only
/// `MODULE.bazel` under a Bazel before 6, which has no bzlmod, or under 6
/// with no `--enable_bzlmod` in the root `.bazelrc`. A `MODULE.bazel` that
/// declares nothing beside a `WORKSPACE` does not count (see
/// [`module_counts`]). Either way every query
/// a run makes loads a different graph from the one the build uses, or a
/// fraction of it.
pub fn mode_misfit(ws: &WorkspaceProbe, major: Option<u32>) -> Option<Diagnostic> {
    let (root, major) = (ws.root.as_ref()?, major?);
    let module = module_counts(root, &ws.markers);
    let stub = !module && ws.markers.iter().any(|m| m == "MODULE.bazel");
    let marker = ws.markers.iter().find(|m| *m != "MODULE.bazel");
    match (module, marker) {
        (false, Some(marker)) if major >= 9 => Some(
            Diagnostic::error(
                Code::E015BazelNotFound,
                format!(
                    "Bazel {major} cannot read this workspace: its dependencies are only in \
                     a {marker} file{}, and Bazel 9 reads no WORKSPACE file, so a query \
                     here loads a fraction of the packages",
                    if stub {
                        " (the MODULE.bazel beside it declares nothing)"
                    } else {
                        ""
                    }
                ),
            )
            .at(marker.clone(), None)
            .with_help(
                "pin a Bazel that reads WORKSPACE in .bazelversion (7.x, or 8.x with \
                 --enable_workspace in .bazelrc), or move the dependencies to MODULE.bazel",
            ),
        ),
        (true, None) if major < 6 => Some(
            Diagnostic::error(
                Code::E015BazelNotFound,
                format!(
                    "Bazel {major} cannot read this workspace: it has only a MODULE.bazel \
                     file, and Bazel before 6 has no bzlmod"
                ),
            )
            .at("MODULE.bazel", None)
            .with_help("pin Bazel 7 or later in .bazelversion"),
        ),
        (true, None) if major == 6 && !bazelrc_enables_bzlmod(root) => Some(
            Diagnostic::error(
                Code::E015BazelNotFound,
                "Bazel 6 cannot read this workspace as it stands: it has only a \
                     MODULE.bazel file, and Bazel 6 reads one only under --enable_bzlmod, \
                     which the root .bazelrc does not leave on",
            )
            .at("MODULE.bazel", None)
            .with_help(
                "pin Bazel 7 or later in .bazelversion, or add `common --enable_bzlmod` \
                     to .bazelrc",
            ),
        ),
        _ => None,
    }
}

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

    let module = module_counts(&root, &p.markers);
    let stub = !module && p.markers.iter().any(|m| m == "MODULE.bazel");
    let workspace = p.markers.iter().any(|m| m != "MODULE.bazel");
    p.mode = match (module, workspace) {
        (true, false) => Some(WorkspaceMode::Bzlmod),
        (false, true) => {
            // Only a WORKSPACE file, and nothing that pins a Bazel: bazelisk
            // runs the latest, and the latest reads no WORKSPACE at all.
            if p.bazelversion.is_none() && !bazeliskrc_pins(&root) {
                let marker = p
                    .markers
                    .iter()
                    .find(|m| *m != "MODULE.bazel")
                    .cloned()
                    .unwrap_or_default();
                p.diagnostics.push(
                    Diagnostic::warning(
                        Code::W016WorkspaceModeAmbiguous,
                        format!(
                            "this workspace has only a {marker} file{} and no .bazelversion, \
                             so which Bazel runs here (and whether it reads {marker} at all) \
                             depends on the machine: bazelisk picks the latest release, and \
                             Bazel 9 reads no WORKSPACE file",
                            if stub {
                                " (the MODULE.bazel beside it declares nothing)"
                            } else {
                                ""
                            }
                        ),
                    )
                    .at(marker, None)
                    .with_help(
                        "pin a Bazel that reads WORKSPACE in .bazelversion: 7.x, or 8.x with \
                         --enable_workspace in .bazelrc",
                    ),
                );
            }
            Some(WorkspaceMode::Workspace)
        }
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
                        "pin the Bazel version in .bazelversion: a run that reads the \
                         other file indexes a different graph. In a WORKSPACE repository \
                         the pin is the fix that sticks, because Bazel 7 and later write \
                         an untracked MODULE.bazel stub there the first time they run, \
                         cambi's own queries included; a bzlmod repository can delete \
                         WORKSPACE instead",
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
///
/// `cfg` is the effective configuration, so a `--bazel-binary` on the command
/// line has already reached `bazel.binary` through the override layer.
pub fn bazel(root: &Path, cfg: &Config) -> BazelProbe {
    let configured = Some(cfg.bazel.binary.as_str()).filter(|b| !b.is_empty());
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

/// Reads the whole configuration stack — `cambium.toml`, `cambium.local.toml`,
/// then `over` — and says what it found. Never fails: an unparsable file is a
/// diagnostic, because `status` still has everything else to print.
///
/// The same [`Config::load_layered`] a run uses, so `status`, `doctor` and
/// `init` see the configuration a run would: a local override or a
/// `--cache-dir` is not a thing a probe may ignore.
pub fn config(
    root: &Path,
    config_path: Option<&Path>,
    over: &Overrides,
) -> (Option<Config>, ConfigProbe) {
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
    match Config::load_layered(root, config_path, over) {
        Ok(layered) => {
            p.hash = Some(hex(&layered.config.hash()));
            p.diagnostics = layered.diagnostics;
            (Some(layered.config), p)
        }
        Err(e) => {
            p.parse_error = Some(e.to_string());
            // Through `EngineError`, so a misplaced local key keeps its own
            // code; the message names the file either way.
            p.diagnostics
                .push(crate::error::EngineError::from(e).diagnostic());
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
            // The lineage-less copies (`baseline.lineage_less_copies`) repeat
            // an index already counted under its lineage.
            p.indexes = Some(
                names
                    .iter()
                    .filter(|n| n.ends_with(".bin") && !n.starts_with("indexes/by-tree/"))
                    .count(),
            );
            let mut lineages: Vec<String> = names.iter().filter_map(|n| lineage_of(n)).collect();
            lineages.sort();
            lineages.dedup();
            p.lineages = lineages;
        }
        Err(crate::store::StoreError::Unsupported(_)) => {
            // A fetch of a name no publisher writes: reachable iff the
            // backend can tell us it is absent.
            match answers(s) {
                Ok(()) => p.reachable = Some(true),
                Err(e) => {
                    p.reachable = Some(false);
                    p.error = Some(e.to_string());
                    p.diagnostics.push(unreachable_diagnostic(&e.to_string()));
                }
            }
        }
        Err(e) => {
            p.reachable = Some(false);
            p.error = Some(e.to_string());
            p.diagnostics.push(unreachable_diagnostic(&e.to_string()));
        }
    }
    p
}

/// Whether `store` answers at all: a fetch of a name no publisher writes,
/// where a miss is an answer and an error is not. Cheaper than a listing on
/// every backend, so a verb about to pay for a build can ask it first.
pub fn answers(store: &dyn ArtifactStore) -> Result<(), crate::store::StoreError> {
    let dest = std::env::temp_dir().join(format!(
        "cambium-probe.{}",
        cambium_index::format::temp_suffix()
    ));
    let r = store.fetch(&absent_probe_name(), &dest).map(|_| ());
    let _ = std::fs::remove_file(&dest);
    r
}

fn unreachable_diagnostic(detail: &str) -> Diagnostic {
    Diagnostic::warning(
        Code::W007StoreFetchFailed,
        format!("the index store could not be reached: {detail}"),
    )
    .with_help(
        "check the path, URL or credentials in [baseline].store; until it answers, a run \
         either fails with exit 5 or, where the backend reads the failure as a miss, builds \
         its own index from scratch (docs/STORE.md, \"Misses and outages\")",
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
                    "mint a fresh lineage with `cambi index build --publish`; on a \
                     directory store `cambi index gc` then removes the old one, and on S3 or \
                     GCS the prefix's lifecycle rule ages it out",
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
                 lineage is skipped, never used. On a directory store `cambi index gc` \
                 removes it now (`--keep-incompatible` keeps it for a configuration still \
                 reading it); on S3 or GCS the prefix's lifecycle rule ages it out",
            ),
        );
    }
    p
}

// ------------------------------------------------------------------ verdict

/// What `doctor` thinks of one check.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Grade {
    /// Nothing to do.
    Ok,
    /// A run works, and something here will surprise someone eventually.
    Warn,
    /// A run cannot work until this is fixed.
    Fail,
}

/// One of `doctor`'s checks: a probe, graded.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Check {
    pub name: &'static str,
    pub level: Grade,
    /// What the check found, in a few words. Always present, `ok` included —
    /// "bazel 7.4.1" is the useful half of a passing Bazel check.
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<Diagnostic>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            level: Grade::Ok,
            detail: detail.into(),
            diagnostic: None,
        }
    }

    /// A check graded by the diagnostic its probe raised: an error diagnostic
    /// is a failure, a warning is a warning. One rule, so that a code's level
    /// and `doctor`'s verdict can never disagree.
    pub fn from(name: &'static str, detail: impl Into<String>, d: Diagnostic) -> Self {
        let level = match d.level {
            crate::diag::Level::Error => Grade::Fail,
            crate::diag::Level::Warning => Grade::Warn,
        };
        Self {
            name,
            level,
            detail: detail.into(),
            diagnostic: Some(d),
        }
    }
}

/// Every probe, graded: `cambi doctor`'s verdict, as a pure function of what
/// the probes found. The order is the order someone fixes things in: you
/// cannot configure a store for a workspace that is not there.
pub fn judge(p: &Probes) -> Vec<Check> {
    let mut checks = Vec::new();

    // ---- workspace ----
    checks.push(match (&p.workspace.root, p.workspace.mode) {
        (None, _) => Check::from(
            "workspace",
            "none found",
            Diagnostic::error(
                Code::E004Bazel,
                "no Bazel workspace above the current directory",
            )
            .with_help("run from inside the workspace, or pass -C <dir>"),
        ),
        (Some(root), Some(WorkspaceMode::Ambiguous)) => Check::from(
            "workspace",
            format!("{} (mode undecided)", root.display()),
            p.workspace
                .diagnostics
                .first()
                .cloned()
                .expect("an ambiguous mode always carries its diagnostic"),
        ),
        (Some(root), Some(WorkspaceMode::Workspace)) if !p.workspace.diagnostics.is_empty() => {
            Check::from(
                "workspace",
                format!("{} (WORKSPACE, no Bazel pinned)", root.display()),
                p.workspace.diagnostics[0].clone(),
            )
        }
        (Some(root), mode) => Check::ok(
            "workspace",
            format!(
                "{} ({})",
                root.display(),
                match mode {
                    Some(WorkspaceMode::Bzlmod) if p.workspace.inert_workspace =>
                        "bzlmod; the WORKSPACE file is inert".to_string(),
                    Some(WorkspaceMode::Bzlmod) => "bzlmod".to_string(),
                    Some(WorkspaceMode::Workspace) if p.workspace.inert_module_bazel =>
                        "WORKSPACE; the MODULE.bazel file is inert".to_string(),
                    Some(WorkspaceMode::Workspace) => "WORKSPACE".to_string(),
                    _ => "no marker file".to_string(),
                }
            ),
        ),
    });

    // ---- bazel ----
    if let Some(b) = &p.bazel {
        checks.push(match (b.diagnostics.first(), &b.version) {
            // The version when there is one: "bazel 9.2.0" is what a
            // version finding is about, and the binary is what a missing one
            // is.
            (Some(d), v) => Check::from(
                "bazel",
                v.clone().unwrap_or_else(|| b.binary.clone()),
                d.clone(),
            ),
            (None, Some(v)) => Check::ok("bazel", v.clone()),
            (None, None) => Check::ok("bazel", b.binary.clone()),
        });
    }

    // ---- config ----
    let config_detail = format!(
        "{}{}",
        p.config.path.display(),
        if p.config.exists {
            ""
        } else {
            " (absent; the defaults apply)"
        }
    );
    checks.push(match p.config.diagnostics.first() {
        Some(d) => Check::from("config", config_detail, d.clone()),
        None => Check::ok("config", config_detail),
    });

    // ---- store ----
    checks.push(match (p.store.configured, p.store.reachable) {
        (false, _) => Check::ok(
            "store",
            "none configured; every run builds its own index (not an error)",
        ),
        (true, Some(true)) => Check::ok(
            "store",
            match p.store.indexes {
                Some(n) => format!(
                    "{}, {n} index{}",
                    p.store.kind,
                    if n == 1 { "" } else { "es" }
                ),
                None => format!("{}, reachable", p.store.kind),
            },
        ),
        (true, _) => Check::from(
            "store",
            format!("{} unreachable", p.store.kind),
            p.store
                .diagnostics
                .first()
                .cloned()
                .unwrap_or_else(|| unreachable_diagnostic("no detail")),
        ),
    });

    // ---- store round trip ----
    if let Some(r) = &p.round_trip {
        checks.push(match r.diagnostics.first() {
            Some(d) => Check::from("store round trip", "publish and fetch back", d.clone()),
            None => Check::ok("store round trip", format!("wrote and read {}", r.name)),
        });
    }

    // ---- position ----
    if let Some(pos) = &p.position {
        let detail = match (&pos.head, pos.plan, pos.distance) {
            (None, ..) => "HEAD could not be resolved".to_string(),
            (Some(h), Plan::Warm, _) => format!("{} has an index", short(h)),
            (Some(h), Plan::Splice, Some(n)) => {
                format!("{} splices from {n} commits back", short(h))
            }
            (Some(h), _, _) => format!("{} would be a cold build", short(h)),
        };
        // A cold position is not a fault: it is what a first run looks like.
        // Only what the walk *complained* about is graded.
        checks.push(match pos.diagnostics.first() {
            Some(d) => Check::from("position", detail, d.clone()),
            None => Check::ok("position", detail),
        });
    }

    // ---- index format, and the lineage it belongs to ----
    if let Some(i) = &p.index {
        checks.push(match i.diagnostics.first() {
            Some(d) => Check::from(
                "index",
                format!("format version {}", i.format_version),
                d.clone(),
            ),
            None => Check::ok(
                "index",
                format!(
                    "format version {}, {} packages, {} rule targets",
                    i.format_version, i.packages, i.targets
                ),
            ),
        });
    }
    if let Some(l) = &p.lineage {
        checks.push(match l.diagnostics.first() {
            Some(d) => Check::from(
                "lineage",
                format!("{} conflicting in the store", l.conflicting.len()),
                d.clone(),
            ),
            None => Check::ok(
                "lineage",
                match &l.current {
                    Some(c) => format!("{}, {} others in the store", &c[..12], l.others.len()),
                    None => "none yet; a cold build mints one".to_string(),
                },
            ),
        });
    }

    checks
}

// ------------------------------------------------------------------- the lot

/// What to probe. The two process- and write-bearing probes are opt-in.
#[derive(Clone, Debug)]
pub struct Options {
    /// Where to start looking for a workspace.
    pub start: PathBuf,
    pub config_path: Option<PathBuf>,
    /// What the command line and the environment said: the layer above the
    /// config files, exactly as a run applies it.
    pub overrides: Overrides,
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
            overrides: Overrides::default(),
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
    let (cfg, config_probe) = config(&root, opts.config_path.as_deref(), &opts.overrides);
    let effective = cfg.clone().unwrap_or_default();

    let bazel_probe = opts.run_bazel.then(|| {
        let mut b = bazel(&root, &effective);
        // First, so the check is graded by it: a Bazel that runs and cannot
        // read this tree is worse than one that is merely untested.
        if let Some(d) = mode_misfit(&ws, b.major) {
            b.diagnostics.insert(0, d);
        }
        b
    });

    // The store's own diagnostics (a `command` backend's failed fetch) go into
    // the store probe, not into a sink nobody drains.
    let sink = Diagnostics::new();
    let backend = crate::store::from_config(&effective.baseline.store, &root, &sink);
    let mut store_probe = store(&effective, backend.as_deref());
    store_probe.diagnostics.extend(sink.drain());

    // `--cache-dir` has already reached the effective config through the
    // override layer, the same way it reaches a run.
    let cache_dir = effective
        .baseline
        .cache_dir
        .clone()
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

/// The leading integer of a version string: `7.4.1` is 7, `8.0.0rc2` is 8,
/// and `latest` or `rolling` is nothing.
pub(crate) fn leading_major(s: &str) -> Option<u32> {
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

pub use crate::inspect::hex;

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

    /// A `.bazeliskrc` that names the version pins it as well as
    /// `.bazelversion` does; with neither, a `WORKSPACE`-only tree is warned
    /// about.
    #[test]
    fn a_workspace_only_tree_needs_a_pin_from_either_file() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "WORKSPACE.bazel", "");
        let p = workspace(d.path());
        assert_eq!(p.mode, Some(WorkspaceMode::Workspace));
        assert_eq!(p.diagnostics.len(), 1);
        assert_eq!(p.diagnostics[0].code, Code::W016WorkspaceModeAmbiguous);
        assert!(p.diagnostics[0].message.contains("WORKSPACE.bazel"));
        write(d.path(), ".bazeliskrc", "USE_BAZEL_VERSION=7.4.1\n");
        assert!(workspace(d.path()).diagnostics.is_empty());
    }

    #[test]
    fn a_bazel_that_cannot_read_the_mode_is_named() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "WORKSPACE", "");
        let ws = workspace(d.path());
        assert_eq!(
            mode_misfit(&ws, Some(9)).map(|d| d.code),
            Some(Code::E015BazelNotFound)
        );
        assert!(mode_misfit(&ws, Some(8)).is_none());
        assert!(
            mode_misfit(&ws, None).is_none(),
            "an unknown version is not judged"
        );
        write(d.path(), "MODULE.bazel", "module(name = \"x\")\n");
        let both = workspace(d.path());
        assert!(
            mode_misfit(&both, Some(9)).is_none(),
            "9 reads the MODULE.bazel"
        );
        assert!(
            mode_misfit(&both, Some(5)).is_none(),
            "5 reads the WORKSPACE"
        );
    }

    /// The comment-only `MODULE.bazel` Bazel writes on its first run beside a
    /// `WORKSPACE` is not a second marker: the tree stays `WORKSPACE`-only,
    /// for the mode and for the Bazel it needs.
    #[test]
    fn a_module_bazel_that_declares_nothing_is_not_a_marker_beside_workspace() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "WORKSPACE", "workspace(name = \"x\")\n");
        write(
            d.path(),
            "MODULE.bazel",
            "#####\n# Bazel now uses Bzlmod by default to manage external dependencies.\n\n#####\n",
        );
        let p = workspace(d.path());
        assert_eq!(p.mode, Some(WorkspaceMode::Workspace));
        assert!(!p.inert_module_bazel && !p.inert_workspace);
        assert_eq!(p.diagnostics.len(), 1);
        assert_eq!(p.diagnostics[0].code, Code::W016WorkspaceModeAmbiguous);
        assert!(
            p.diagnostics[0].message.contains("only a WORKSPACE file"),
            "{}",
            p.diagnostics[0].message
        );
        assert!(p.diagnostics[0].message.contains("declares nothing"));
        let misfit = mode_misfit(&p, Some(9)).expect("9 reads only the stub");
        assert_eq!(misfit.code, Code::E015BazelNotFound);
        assert!(
            misfit.message.contains("declares nothing"),
            "{}",
            misfit.message
        );
        assert!(mode_misfit(&p, Some(7)).is_none());

        // With no WORKSPACE, an empty MODULE.bazel is the tree's only marker,
        // and a bzlmod tree with no dependencies is a real one.
        let m = tempfile::tempdir().unwrap();
        write(m.path(), "MODULE.bazel", "");
        assert_eq!(workspace(m.path()).mode, Some(WorkspaceMode::Bzlmod));
    }

    /// Bazel 6 reads `MODULE.bazel` only under `--enable_bzlmod`, the last
    /// setting in the root `.bazelrc` winning; turning it off is not turning
    /// it on.
    #[test]
    fn bazel_6_needs_bzlmod_turned_on_and_left_on() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "MODULE.bazel", "module(name = \"x\")\n");
        let misfit = |rc: &str| {
            write(d.path(), ".bazelrc", rc);
            mode_misfit(&workspace(d.path()), Some(6)).is_some()
        };
        assert!(misfit(""));
        assert!(!misfit("common --enable_bzlmod\n"));
        assert!(!misfit("build --enable_bzlmod=true\n"));
        assert!(misfit("common --noenable_bzlmod\n"));
        assert!(misfit("common --enable_bzlmod=false\n"));
        assert!(misfit("common --enable_bzlmod\nbuild --noenable_bzlmod\n"));
        assert!(misfit("# common --enable_bzlmod\n"));
    }

    #[test]
    fn both_files_and_no_pin_is_ambiguous_and_says_so() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "MODULE.bazel", "module(name = \"x\")\n");
        write(d.path(), "WORKSPACE", "");
        let p = workspace(d.path());
        assert_eq!(p.mode, Some(WorkspaceMode::Ambiguous));
        assert_eq!(p.diagnostics.len(), 1);
        assert_eq!(p.diagnostics[0].code, Code::W016WorkspaceModeAmbiguous);
        assert!(p.diagnostics[0].help.is_some());
    }

    /// Deleting `MODULE.bazel` from a WORKSPACE repository does not stick:
    /// Bazel 7 and later write an untracked stub back the next time they run
    /// there, cambi's own queries included. The help leads with the pin and
    /// says why, and offers deletion only where it lasts.
    #[test]
    fn the_ambiguous_mode_help_says_the_pin_is_what_sticks() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "MODULE.bazel", "module(name = \"x\")\n");
        write(d.path(), "WORKSPACE", "");
        let p = workspace(d.path());
        let help = p.diagnostics[0].help.as_deref().unwrap();
        assert!(
            help.starts_with("pin the Bazel version in .bazelversion"),
            "{help}"
        );
        assert!(help.contains("untracked MODULE.bazel stub"), "{help}");
        assert!(help.contains("Bazel 7 and later"), "{help}");
        assert!(
            !help.contains("delete the file the build does not use"),
            "deleting the stub does not last: {help}"
        );
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
        let (cfg, p) = config(d.path(), None, &Overrides::default());
        assert!(cfg.is_none());
        assert!(p.exists && p.hash.is_none());
        assert_eq!(p.diagnostics[0].code, Code::E002Config);
    }

    #[test]
    fn a_missing_config_is_the_defaults_and_a_hash() {
        let d = tempfile::tempdir().unwrap();
        let (cfg, p) = config(d.path(), None, &Overrides::default());
        assert_eq!(cfg, Some(Config::default()));
        assert!(!p.exists);
        assert_eq!(p.hash, Some(hex(&Config::default().hash())));
    }

    /// The probes read what a run reads: the local file and the flags reach
    /// the effective configuration, and a local file that oversteps keeps
    /// its own code rather than being reported as a parse failure.
    #[test]
    fn the_config_probe_sees_the_local_file_and_the_overrides() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            crate::config::LOCAL_CONFIG_FILE,
            "[baseline]\ncompress_indexes = false\n",
        );
        let over = Overrides {
            bazel_binary: None,
            cache_dir: Some((
                d.path().join("cache"),
                crate::config::Origin::Flag {
                    name: "--cache-dir".into(),
                },
            )),
        };
        let (cfg, p) = config(d.path(), None, &over);
        let cfg = cfg.expect("loads");
        assert!(!cfg.baseline.compress_indexes, "the local file applied");
        assert_eq!(cfg.baseline.cache_dir, Some(d.path().join("cache")));
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);

        write(
            d.path(),
            crate::config::LOCAL_CONFIG_FILE,
            "[invalidators]\npaths = [\"x\"]\n",
        );
        let (cfg, p) = config(d.path(), None, &Overrides::default());
        assert!(cfg.is_none());
        assert_eq!(p.diagnostics[0].code, Code::E014LocalOverrideKey);
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
        // Its lineage-less copy is the same index, not a second one.
        backend.publish("indexes/by-tree/abc.bin", &blob).unwrap();
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

    /// Everything absent, which is the shape outside a workspace: the grading
    /// has to survive it, because that is exactly when someone runs `doctor`.
    fn nowhere() -> Probes {
        collect(&Options::new(
            tempfile::tempdir().unwrap().path().join("nowhere"),
        ))
    }

    #[test]
    fn no_workspace_is_a_failure_with_a_code() {
        let checks = judge(&nowhere());
        let ws = &checks[0];
        assert_eq!(ws.name, "workspace");
        assert_eq!(ws.level, Grade::Fail);
        assert_eq!(ws.diagnostic.as_ref().unwrap().code, Code::E004Bazel);
        assert!(ws.diagnostic.as_ref().unwrap().help.is_some());
    }

    /// A store nobody configured is not a fault: a developer with no store
    /// builds their own indexes and `doctor` must not tell them they are broken.
    #[test]
    fn an_unconfigured_store_is_ok() {
        let checks = judge(&nowhere());
        let store = checks
            .iter()
            .find(|c| c.name == "store")
            .expect("a store check");
        assert_eq!(store.level, Grade::Ok);
        assert!(store.diagnostic.is_none());
    }

    #[test]
    fn a_warning_diagnostic_grades_warn_and_an_error_grades_fail() {
        let warn = Check::from(
            "x",
            "d",
            Diagnostic::warning(Code::W017BazelVersionUnsupported, "old"),
        );
        assert_eq!(warn.level, Grade::Warn);
        let fail = Check::from("x", "d", Diagnostic::error(Code::E015BazelNotFound, "gone"));
        assert_eq!(fail.level, Grade::Fail);
        assert!(Grade::Fail > Grade::Warn && Grade::Warn > Grade::Ok);
    }

    #[test]
    fn every_check_says_something_even_when_it_passes() {
        for c in judge(&nowhere()) {
            assert!(!c.detail.is_empty(), "{} has no detail", c.name);
        }
    }

    #[test]
    fn a_version_string_yields_its_major_however_it_is_spelled() {
        assert_eq!(leading_major("7.4.1"), Some(7));
        assert_eq!(leading_major(" 8.0.0rc2 "), Some(8));
        assert_eq!(leading_major("last_green"), None);
    }
}
