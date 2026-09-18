//! Reading a `.bazelrc` diff instead of surrendering to it.
//!
//! `.bazelrc` is a default invalidator, and for a good reason: a line in it can
//! change what `bazel query` sees: `--repo_env`, `--registry`, `--enable_bzlmod`,
//! an `import` of another file, a `--config` every later command names. Any of
//! those moves the graph, and nothing in the index records which flags produced
//! it, so the only sound answer is "everything changed".
//!
//! But most `.bazelrc` edits are not that. Across the real subjects, the commits
//! whose *only* global reason is `.bazelrc` are overwhelmingly remote-cache
//! endpoints, `--jobs`, compiler flags, test-output verbosity and BES
//! bookkeeping: flags that select how the build is executed and reported,
//! never what is loaded. This module reads the whole rc chain at both ends of
//! the range, works out which (command, config, flag) triples a `bazel query`
//! would actually read at each end, and lets a run past only when *every*
//! triple that differs is on an allowlist of flags known not to reach loading.
//! Everything else (an unknown flag, a startup option, an `import` it cannot
//! resolve, a non-UTF-8 file, the file appearing or disappearing) is still
//! global.
//!
//! # What reaches the query
//!
//! The index is `bazel query //...`, so the question is always "would that
//! query load or report something different". Three scopes decide whether a
//! triple reaches it at all, before any flag is looked at:
//!
//! - **Command.** Bazel applies an rc line to the command it names and to the
//!   commands that inherit from it. `query` inherits from nothing, so only
//!   `common`, `always` and `query` lines reach it; `build`, `test`, `run`,
//!   `coverage`, `cquery` and the rest never do, whatever they carry. A
//!   `build --enable_bzlmod` is a fact about builds, not about the graph the
//!   index holds. `startup` lines reach every command and are never
//!   classified.
//! - **Config.** A `command:name` line applies only when `--config=name` is
//!   in effect for that command. The screen computes the configs in effect
//!   for the query at each end: the ones `[bazelrc].configs` and
//!   `[bazel].extra_query_flags` pass on the command line, plus every
//!   `--config=` a line that is itself in effect names, to a fixpoint, plus
//!   the host-platform names when `--enable_platform_specific_config` is in
//!   effect (all five, since which host runs the query is not modelled).
//!   A triple under any other config is skipped. `--config` itself is never
//!   a changed flag: it is expanded, and the flags it pulls in are what get
//!   screened, so the reason names `common:ci --repo_env` rather than a bare
//!   `--config`. A `--config=name` in effect that no rc file defines for any
//!   command is unknown: Bazel refuses it, and it may be defined in a file
//!   the repository does not track.
//! - **Import.** An `import` or `try-import` is textual inclusion, so the
//!   chain is followed at each end and the imported files' triples are
//!   screened like the root's. A `try-import` whose target is missing at an
//!   end contributes nothing there, which is exactly what Bazel does; an
//!   `import` whose target is missing would fail Bazel, and one naming a file
//!   outside the repository cannot be read, so either one is unknown when the
//!   line itself moved.
//! - **Startup.** `[bazel].startup_flags` decide which files are rc files at
//!   all: `--bazelrc=` adds one, and `--noworkspace_rc` tells Bazel not to
//!   read `%workspace%/.bazelrc`, nor therefore anything it imports. Under
//!   it the root rc is not an rc file for this workspace, and an edit to it
//!   is an ordinary source change, exactly like a nested workspace's
//!   `.bazelrc` (and exempt from the `.bazelrc` invalidator for the same
//!   reason).
//!
//! # The allowlist
//!
//! The allowlist is the whole safety argument for what is left, so it is
//! written as data with a reason per entry, and it errs towards "unknown": a
//! flag that is merely *probably* irrelevant costs one over-invalidation if it
//! is left off, and a missed target if it is wrongly put on. The criterion is
//! loading: whether the flag can change which packages load, which targets
//! exist, or which edges `bazel query` reports. Analysis-phase effects
//! (`select()` resolution, toolchain choice, compiler command lines) are
//! declined by design, as docs/ROADMAP.md says, and the flags that only have
//! those are on the list. [`EVIDENCE`] records every flag that fired
//! `CAMBI-W004` across the subject histories, with its verdict and reasoning.
//!
//! # Starlark flags
//!
//! One family off the allowlist can still be answered exactly, because it is
//! not a flag at all but a *target*: a Starlark build setting, written
//! `--@envoy//bazel:crypto=boringssl` or `--//bazel:foo=bar`. [`screen_chain`]
//! hands those back to the engine instead of calling them unknown; the engine
//! resolves each against the index and either seeds its node or keeps the
//! global. The reasoning it relies on:
//!
//! - A `config_setting(flag_values = {"//bazel:crypto": "boringssl"})` names
//!   the flag through a label-keyed attribute, so `bazel query` records an edge
//!   from the config_setting to the flag, and every `select()` key is an edge
//!   from the selecting rule to the config_setting. Seeding the flag's node
//!   into the direct set and running the ordinary reverse walk therefore
//!   reaches every target whose definition the value can move, exactly as the
//!   externals stage does for a moved repository's leaf nodes. The same goes
//!   for a rule reading the setting through `attr.label(default = ...)`: the
//!   default is an edge.
//! - A Starlark *transition* reading the flag (`transition(inputs = [...])`) is
//!   not an edge. Its `inputs`/`outputs` are string literals in a `.bzl`, so
//!   the engine looks for the label in the text of every `.bzl` in the stored
//!   load closure; a hit means the walk cannot be trusted and the run stays
//!   global, naming the file.
//! - A setting in an external repository keeps its global: the index does not
//!   cover that repository's graph, so what inside it reads the setting cannot
//!   be seen. Only the main repository's own settings are resolved. (Its value
//!   cannot reach loading either, but `bazel query` loads the flag's package
//!   while parsing the option, so a renamed or removed setting can change
//!   which packages load and can fail the query; W015 is the sound side.)
//! - A setting under a config that is in effect is handed back like an
//!   unconditional one; under any other config it is skipped like any other
//!   triple.
//!
//! # `--registry`
//!
//! `--registry` chooses where module resolution reads from, and the resolved
//! state is `MODULE.bazel.lock`. A registry edit is therefore a lock-file
//! question: the screen reports it to the run rather than calling it unknown,
//! and the run asks the externals stage. When the lockfile moved in the same
//! diff and the stage localised it, the lock diff already names the modules
//! whose registry metadata moved and the rc line adds nothing; when bzlmod is
//! off the line is inert; otherwise the lock did not move with the registry
//! and the stage escalates, since what the new registries serve cannot be
//! read from the tree.

use std::collections::{BTreeMap, BTreeSet};

use cambium_index::label::{Label, Repo};

/// Rc commands whose lines a `bazel query` reads. `query` inherits from no
/// other command, so nothing under `build` or `test` can reach it.
const QUERY_COMMANDS: &[&str] = &["common", "always", "query"];

/// The config names `--enable_platform_specific_config` expands to, one per
/// host Bazel runs on. Which host runs the query is not modelled, so all of
/// them are in effect when the flag is.
const PLATFORM_CONFIGS: &[&str] = &["linux", "macos", "windows", "freebsd", "openbsd"];

/// The rc chain Bazel reads for this workspace at one revision: `.bazelrc` at
/// the root, any file in the repository a `--bazelrc=` startup flag names,
/// and everything those reach through `import` / `try-import`, each parsed.
///
/// Only these are screened as rc files. A repository routinely holds other
/// files with the name (a nested test workspace's `.bazelrc`, a fixture) and
/// Bazel never reads them for this workspace, so an edit to one is an ordinary
/// source change, not a global invalidation. envoy's
/// `bazel/tests/external/.bazelrc` is the case that made this a rule: it moved
/// twice in twenty commits and each time cost a whole rebuild for nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chain {
    /// Repository-relative path -> the file, parsed.
    pub files: BTreeMap<String, Bazelrc>,
    /// Import lines (`import X` / `try-import X`) whose target contributes
    /// nothing at this end and whose absence is not harmless: an `import` of a
    /// file that is missing (Bazel fails), or either keyword naming a file
    /// outside the repository (it cannot be read here). Keyed by the line,
    /// with the file that carries it.
    pub unresolved: BTreeMap<String, String>,
}

/// The workspace rc file, `%workspace%/.bazelrc`, which Bazel reads unless
/// the startup option `--noworkspace_rc` says not to.
pub const WORKSPACE_RC: &str = ".bazelrc";

/// Whether Bazel reads [`WORKSPACE_RC`] under these startup flags.
///
/// `--noworkspace_rc` turns it off and `--workspace_rc` back on, the last
/// one winning as in Bazel's startup parser (both are nullary: neither takes
/// a value). Every other rc source, a `--bazelrc=` file and what it imports,
/// is read regardless. The benchmark runs apollo this way, because its own
/// rc carries a query flag the pinned Bazel refuses; under it the root rc is
/// not an rc file for this workspace at all, and an edit to it is an
/// ordinary source change.
pub fn reads_workspace_rc(startup_flags: &[String]) -> bool {
    startup_flags.iter().fold(true, |on, f| match f.as_str() {
        "--noworkspace_rc" => false,
        "--workspace_rc" => true,
        _ => on,
    })
}

/// Reads the rc chain at one revision.
///
/// `read` returns a file's text at that revision, or `None` when it does not
/// exist there or is not text. Import targets are `%workspace%`-relative or
/// absolute in Bazel; an absolute path under `root` is mapped into the
/// repository and any other absolute path is outside it. A bare relative path
/// is taken as repository-relative, which is the only reading under which it
/// could name a file in the diff. The chain starts from [`WORKSPACE_RC`]
/// unless the startup flags say Bazel does not read it
/// ([`reads_workspace_rc`]), and from every `--bazelrc=` file inside the
/// repository.
pub fn chain(
    root: &std::path::Path,
    startup_flags: &[String],
    read: &dyn Fn(&str) -> Option<String>,
) -> Chain {
    let mut out = Chain::default();
    let mut queue: Vec<String> = Vec::new();
    if reads_workspace_rc(startup_flags) {
        queue.push(WORKSPACE_RC.to_string());
    }
    for f in startup_flags {
        if let Some(p) = f.strip_prefix("--bazelrc=") {
            if let Some(rel) = repo_relative(root, p) {
                queue.push(rel);
            }
        }
    }
    let mut seen = BTreeSet::new();
    while let Some(path) = queue.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Some(text) = read(&path) else { continue };
        let rc = parse(&text);
        for import in &rc.imports {
            let (keyword, target) = import.split_once(' ').unwrap_or((import.as_str(), ""));
            match repo_relative(root, target) {
                Some(rel) => {
                    if read(&rel).is_some() {
                        queue.push(rel);
                    } else if keyword == "import" {
                        out.unresolved.insert(import.clone(), path.clone());
                    }
                }
                None => {
                    out.unresolved.insert(import.clone(), path.clone());
                }
            }
        }
        out.files.insert(path, rc);
    }
    out
}

/// The paths of the rc files Bazel reads for this workspace at one revision;
/// [`chain`] without the contents.
pub fn reachable(
    root: &std::path::Path,
    startup_flags: &[String],
    read: &dyn Fn(&str) -> Option<String>,
) -> BTreeSet<String> {
    chain(root, startup_flags, read).files.into_keys().collect()
}

/// An rc path as Bazel would write it, as a repository-relative path, or
/// `None` when it names a file outside the repository.
fn repo_relative(root: &std::path::Path, target: &str) -> Option<String> {
    let rel = if let Some(rest) = target.strip_prefix("%workspace%/") {
        rest.to_string()
    } else if target.starts_with('/') {
        std::path::Path::new(target)
            .strip_prefix(root)
            .ok()?
            .to_string_lossy()
            .into_owned()
    } else {
        target.to_string()
    };
    let rel = rel.trim_start_matches("./").to_string();
    (!rel.is_empty()).then_some(rel)
}

/// What the screen knows about the invocation it screens for.
#[derive(Clone, Copy, Debug, Default)]
pub struct Invocation<'a> {
    /// `--config=` names the index query runs with on its command line, on
    /// top of whatever the rc chain itself names: `[bazelrc].configs` and any
    /// `--config=` in `[bazel].extra_query_flags`.
    pub configs: &'a [String],
    /// `[bazelrc].allowlist_extra`.
    pub extra_allowlist: &'a [String],
}

/// A changed flag whose name is a label: a Starlark build setting
/// (`--@envoy//bazel:crypto=boringssl`, `--//bazel:foo=bar`).
///
/// Unlike `--define` or a native flag, this one *is* a target, and the index
/// records every edge into it: `config_setting(flag_values = ...)` names it,
/// and a rule's `select()` names the config_setting. So the engine can answer
/// what a change to it reaches instead of giving up; see
/// [`crate::engine::Engine::compute`] and the module doc's "Starlark flags".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelFlag {
    pub key: FlagKey,
    /// The label as written in the rc file, dashes and `no` prefix removed.
    pub label: String,
}

/// A `--registry` triple that differs between the two ends, and the rc file
/// that carries it; see the module doc's "`--registry`".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryChange {
    pub path: String,
    pub key: FlagKey,
}

/// The first thing in the diff the screen could not clear, and the rc file
/// it sits in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocker {
    pub path: String,
    pub why: Unknown,
}

/// The result of reading a `.bazelrc` diff: how many triples the query reads
/// changed, which of them were label-valued Starlark flags (resolvable
/// against the index), whether `--registry` moved, and the first thing that
/// was none of those and not allowlisted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Screen {
    pub changed_flags: usize,
    pub label_flags: Vec<LabelFlag>,
    pub registry: Option<RegistryChange>,
    pub unknown: Option<Blocker>,
}

/// The first thing in a `.bazelrc` diff the screen could not clear.
///
/// Its `Display` is the tail of the run's `global_reason`, and is pinned by
/// the tests in [`crate::reason`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unknown {
    /// An `import` / `try-import` line moved whose target cannot be read at
    /// the end it applies to: a plain `import` of a missing file, or either
    /// keyword naming a file outside the repository.
    ImportChanged { import: String },
    /// A `startup` option changed; those are never classified.
    StartupFlag { name: String },
    /// A `--config=name` the query reads moved, and no rc file defines
    /// `name` for any command.
    ConfigUndefined { command: String, config: String },
    /// A native flag the query reads that is not on the loading-irrelevant
    /// allowlist. `config` is the `command:config` scope it was set under,
    /// when it was set under one that is in effect.
    NotAllowlisted {
        command: String,
        config: Option<String>,
        name: String,
    },
}

/// `command` or `command:config`, as an rc file writes the scope.
fn scope(command: &str, config: Option<&str>) -> String {
    match config {
        Some(c) => format!("{command}:{c}"),
        None => command.to_string(),
    }
}

impl std::fmt::Display for Unknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unknown::ImportChanged { import } => {
                write!(
                    f,
                    "`{import}` changed; an imported rc file can carry anything"
                )
            }
            Unknown::StartupFlag { name } => write!(
                f,
                "startup --{name} changed, and startup options are never classified"
            ),
            Unknown::ConfigUndefined { command, config } => write!(
                f,
                "{command} --config={config} changed, and no rc file defines `{config}` for \
                 any command"
            ),
            Unknown::NotAllowlisted {
                command,
                config,
                name,
            } => write!(
                f,
                "{} --{name} changed, not on the loading-irrelevant allowlist",
                scope(command, config.as_deref())
            ),
        }
    }
}

/// What a `.bazelrc` diff means for the run, without an index to resolve
/// label flags against.
///
/// Kept for the opt-in evidence test that classifies a subject checkout's
/// every `.bazelrc` edit; the engine itself uses [`screen_chain`], which can
/// do better with a label flag than call it unknown.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every changed flag is on the loading-irrelevant allowlist. The number of
    /// changed triples is carried for the stats line.
    Safe { changed_flags: usize },
    /// Something changed that could move the graph; the string says what.
    Unknown(String),
}

/// One allowlist entry: a flag-name pattern (`*` only as a trailing wildcard)
/// and why it cannot affect loading.
struct Allowed {
    pattern: &'static str,
    _why: &'static str,
}

const fn a(pattern: &'static str, why: &'static str) -> Allowed {
    Allowed { pattern, _why: why }
}

/// Flags whose value cannot change which packages load, which targets exist,
/// or which edges `bazel query` reports.
///
/// Deliberately *not* here, with the reason:
///
/// - `--repository_cache`, `--override_repository`, `--override_module`: they
///   change which external code is fetched, and external code defines rules.
///   (`--registry` is not here either, but it is routed to the externals
///   stage rather than called unknown; see the module doc.)
/// - `--repo_env`, and `--action_env=NAME=VALUE`: repository rules and module
///   extensions read them, and a repository rule writes BUILD files.
/// - `--workspace_status_command`: stamping is invisible to the graph today,
///   but the command's output reaches actions, and the margin is not worth it.
/// - `--output_user_root` and every other startup option: startup options
///   restart the server and can carry `--bazelrc`/`--nosystem_rc`, which change
///   which files are read at all. Startup lines are never classified safe.
/// - `--@repo//flag=`, `--//flag=`: build settings, handed to the flag
///   resolver instead. `--platforms`, `--cpu`, `--compilation_mode`,
///   `--enable_bzlmod`, `--enable_workspace`, `--deleted_packages`,
///   `--package_path`, `--incompatible_enable_proto_toolchain_resolution` and
///   every other `--incompatible_*` and `--experimental_*` flag not named
///   below: these are the graph, or a Starlark semantics option every `.bzl`
///   is evaluated under. They are unknown by falling off this list; there is
///   no separate deny list to keep in sync. `--config` never reaches this
///   list: it is expanded first.
const ALLOWLIST: &[Allowed] = &[
    a(
        "remote_*",
        "remote cache/executor endpoints, headers, timeouts and retries: execution transport only",
    ),
    a("disk_cache", "where action outputs are cached locally"),
    a("jobs", "how many actions run at once"),
    a("local_resources", "scheduling budget for local actions"),
    a("local_cpu_resources", "scheduling budget for local actions"),
    a("local_ram_resources", "scheduling budget for local actions"),
    a("local_test_jobs", "how many tests run at once"),
    a("test_output", "how test output is printed"),
    a("test_summary", "how the test summary is printed"),
    a("test_timeout", "how long a test may run"),
    a(
        "test_verbose_timeout_warnings",
        "whether a timeout warning is printed",
    ),
    a("flaky_test_attempts", "how often a failed test is retried"),
    a(
        "cache_test_results",
        "whether cached test results are reused",
    ),
    a("runs_per_test", "how often a test is run"),
    // Test *selection*: which of the already-built tests `bazel test` runs.
    // No target's definition changes; cambium's answer is the set of affected
    // tests, and a runner applies these filters to that set on top.
    a(
        "test_tag_filters",
        "which tests are selected to run, by tag",
    ),
    a(
        "test_size_filters",
        "which tests are selected to run, by size",
    ),
    a(
        "test_timeout_filters",
        "which tests are selected to run, by timeout",
    ),
    a(
        "test_lang_filters",
        "which tests are selected to run, by language",
    ),
    a(
        "build_tests_only",
        "whether non-test targets are built at all",
    ),
    a("show_progress*", "progress display"),
    a("show_loading_progress", "progress display"),
    a("show_timestamps", "progress display"),
    a("show_result", "how many results are printed at the end"),
    a("progress_*", "progress display"),
    a("color", "terminal colour"),
    a("curses", "terminal cursor control"),
    a(
        "verbose_failures",
        "whether failing command lines are printed",
    ),
    a("announce_rc", "whether the rc lines in effect are printed"),
    a("bes_*", "build event service upload"),
    a("build_event_*", "build event stream output"),
    a("experimental_ui_*", "terminal UI"),
    a("ui_*", "terminal UI"),
    a(
        "keep_going",
        "whether the build stops at the first failure; the graph is the same either way",
    ),
    a(
        "sandbox_*",
        "how actions are isolated when they run; analysis has already happened",
    ),
    a("spawn_strategy", "which executor runs an action"),
    a("strategy", "which executor runs an action"),
    a("genrule_strategy", "which executor runs a genrule"),
    a("worker_*", "persistent worker pool behaviour"),
    a("profile", "where the JSON profile is written"),
    a("memory_profile", "where the memory profile is written"),
    a(
        "build_metadata",
        "key/value metadata attached to the build event stream",
    ),
    a("max_idle_secs", "how long an idle server lives"),
    a("io_nice_level", "process scheduling priority"),
    a(
        "watchfs",
        "whether the server watches the filesystem for changes",
    ),
    // Analysis-only configuration, from the subject histories (`EVIDENCE`
    // carries the full reasoning per flag). None of these can be read by a
    // BUILD file, a macro, a repository rule or a module extension, and
    // `bazel query` reports every `select()` branch whatever they say.
    a(
        "define",
        "config_setting(define_values) and Make variables, both resolved at analysis; the \
         query graph carries every select() branch",
    ),
    a(
        "features",
        "the default feature set of every configured target, applied at analysis",
    ),
    a(
        "host_features",
        "the default feature set of every configured target, applied at analysis",
    ),
    a("copt", "C/C++ compile action command lines"),
    a("cxxopt", "C++ compile action command lines"),
    a("conlyopt", "C compile action command lines"),
    a("linkopt", "link action command lines"),
    a(
        "per_file_copt",
        "C/C++ compile action command lines, per file",
    ),
    a(
        "host_copt",
        "C/C++ compile action command lines, exec configuration",
    ),
    a(
        "host_cxxopt",
        "C++ compile action command lines, exec configuration",
    ),
    a(
        "host_conlyopt",
        "C compile action command lines, exec configuration",
    ),
    a(
        "host_linkopt",
        "link action command lines, exec configuration",
    ),
    a(
        "host_per_file_copt",
        "C/C++ compile action command lines, per file, exec configuration",
    ),
    a(
        "dynamic_mode",
        "static versus dynamic linking of cc_binary/cc_test, decided at analysis",
    ),
    a(
        "java_language_version",
        "which java_toolchain analysis selects",
    ),
    a(
        "tool_java_language_version",
        "which java_toolchain analysis selects for tools",
    ),
    a(
        "java_runtime_version",
        "which java_runtime analysis selects",
    ),
    a(
        "tool_java_runtime_version",
        "which java_runtime analysis selects for tools",
    ),
    a(
        "incompatible_default_to_explicit_init_py",
        "whether py_binary runfiles get synthesised __init__.py files, decided at analysis; \
         a no-op from Bazel 9",
    ),
];

/// What a flag can do to the graph the index holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Cannot change what `bazel query` loads or reports; allowlisted.
    Irrelevant,
    /// Can; stays unknown, and global.
    Relevant,
    /// Not a flag the allowlist decides: expanded, resolved or routed by a
    /// rule of the screen's own.
    Routed,
}

/// One flag that fired `CAMBI-W004` on a subject history, and what was
/// decided about it.
pub struct Evidence {
    /// The flag name as an rc file writes it, dashes removed. `no`-prefixed
    /// spellings are the same flag.
    pub flag: &'static str,
    pub verdict: Effect,
    /// Where it fired: subject and history step, from the box runs.
    pub fired_on: &'static str,
    /// The Bazel-side reasoning. Sources are the 8.7.0 and 9.2.0 trees and
    /// `bazel help <command> --long`.
    pub why: &'static str,
}

const fn e(
    flag: &'static str,
    verdict: Effect,
    fired_on: &'static str,
    why: &'static str,
) -> Evidence {
    Evidence {
        flag,
        verdict,
        fired_on,
        why,
    }
}

/// Every flag that fired `CAMBI-W004` across the five subject histories
/// (bazel, sourcegraph, apollo, envoy, xla; 300 steps each), classified.
///
/// Two independent classifications were made against the Bazel sources and
/// reconciled; this table is the record, and a test holds the allowlist to
/// it: every `Irrelevant` entry is allowlisted and no `Relevant` one is.
/// docs/OVER_REPORTING.md carries the same table for readers of the docs.
pub const EVIDENCE: &[Evidence] = &[
    e(
        "registry",
        Effect::Routed,
        "envoy 293, 295 and six registry pin bumps in the 300-step history; envoy's nested \
         api/.bazelrc and bazel/tests/external/.bazelrc before those stopped being rc files",
        "RepositoryOptions; help tags changes_inputs, under 'Bzlmod output and semantics'; \
         accepted by `bazel query`. It selects the module registries resolution reads, so a \
         list or pin change can change resolved module versions, patches and MODULE.bazel \
         contents, hence which external repositories exist and what their BUILD files hold. \
         Loading-relevant, and the resolved state is `MODULE.bazel.lock`: since 7.2 the lock \
         records a hash per registry file read, so a re-pointed registry re-runs resolution \
         and rewrites the lock. Routed to the externals stage as a lock-file question rather \
         than allowlisted or called unknown.",
    ),
    e(
        "enable_bzlmod",
        Effect::Relevant,
        "envoy 293 (added with --noenable_workspace), envoy 296 (removed); mobile/.bazelrc",
        "Help tags loading_and_analysis; accepted by `bazel query` in 8.7 (a graveyard no-op \
         in 9.2, where WORKSPACE is gone). It selects the external dependency system: which \
         repositories exist, canonical names and the repository mapping, whether WORKSPACE \
         or MODULE.bazel is evaluated. Every external label's package changes. The parser \
         keeps `noenable_bzlmod` and `enable_bzlmod` as separate keys, so one flip is \
         several changed triples; the verdict is the same for each.",
    ),
    e(
        "enable_workspace",
        Effect::Relevant,
        "envoy 293 and 296, beside enable_bzlmod",
        "The other half of the same switch: whether the WORKSPACE closure declares \
         repositories at all. Loading-relevant for the same reason.",
    ),
    e(
        "incompatible_enable_proto_toolchain_resolution",
        Effect::Relevant,
        "envoy 80 (default flipped to true), envoy 235 (a duplicated line, value-neutral)",
        "A StarlarkSemantics option (BuildLanguageOptions), help tags loading_and_analysis, \
         accepted by `bazel query`. Semantics are part of every .bzl load key, so every .bzl \
         is re-evaluated, and protobuf's toolchain_helpers.bzl reads it at .bzl top level to \
         choose between legacy `_proto_compiler` attributes and toolchains on proto_library, \
         cc_proto_library and friends. Those implicit attributes are query-visible edges, so \
         `bazel query deps()` changes. The step-235 instance was `['']` -> `['', 'true']`, \
         the same effective value twice, which the multiset diff cannot tell from a change; \
         the flag class is loading-relevant regardless.",
    ),
    e(
        "incompatible_default_to_explicit_init_py",
        Effect::Irrelevant,
        "envoy 235 (a duplicated `build` line, no value change)",
        "Help tags affects_outputs; the Python configuration fragment reads it at analysis \
         to decide whether __init__.py files are synthesised into runfiles. Not accepted by \
         `bazel query`; a deprecated no-op in 9.2 (BazelRulesModule graveyard). Neither \
         package loading nor the target graph sees it.",
    ),
    e(
        "per_file_copt",
        Effect::Irrelevant,
        "envoy 235 (`common` and `build` lines)",
        "CppOptions; help tags action_command_lines, affects_outputs; not accepted by `bazel \
         query`, which drops unsupported `common` options. Appends options to C++ compile \
         command lines for files matching a regex. Execution only.",
    ),
    e(
        "define",
        Effect::Irrelevant,
        "envoy 235 (build:fuzz-coverage), envoy 295 (common:openssl)",
        "CoreOptions; help tags changes_inputs, affects_outputs. `bazel query` accepts it only \
         because QueryCommand carries CoreOptions for --action_env, and never evaluates \
         select()/config_setting or Make variables, its only consumers, both of which happen \
         at analysis. Cambium passes --proto:flatten_selects=false and walks every select() \
         branch, so no package, target or edge in the index depends on the value. Which \
         branch is *built* is analysis-phase configuration, declined by design.",
    ),
    e(
        "features",
        Effect::Irrelevant,
        "xla 228 (`common --features=-external_include_paths` removed, with host_features)",
        "CoreOptions defaultFeatures; help tags changes_inputs, affects_outputs. Consumed when \
         a rule's effective feature set (package features + rule features + these) is \
         computed at analysis for the toolchain's feature configuration; for \
         -external_include_paths that changes C++ compile flags. Query accepts it only \
         through CoreOptions; no package, target or edge changes.",
    ),
    e(
        "host_features",
        Effect::Irrelevant,
        "xla 228",
        "CoreOptions hostFeatures; the exec-configuration twin of --features, same reasoning.",
    ),
    e(
        "dynamic_mode",
        Effect::Irrelevant,
        "xla 233 (`--dynamic_mode=off` removed under common:rocm_ci_hermetic)",
        "CppOptions; help tags loading_and_analysis, affects_outputs. The loading_and_analysis \
         tag is Bazel's coarse 'changes the configured graph' bucket, applied to many \
         analysis-only C++ options; this one is read by cc_binary/cc_test through the cpp \
         fragment to choose static versus dynamic linking. Not accepted by `bazel query`; no \
         implicit output or attribute set depends on it.",
    ),
    e(
        "java_language_version",
        Effect::Irrelevant,
        "bazel 50 (`common --java_language_version=21` -> 25, with tool_java_language_version)",
        "JavaOptions, no effect tags in help. Feeds JavaConfiguration and java_toolchain \
         selection through config_settings on the java fragment at analysis; not accepted by \
         `bazel query`, and module extensions cannot read it. Which toolchain is selected \
         changes, not what loads.",
    ),
    e(
        "tool_java_language_version",
        Effect::Irrelevant,
        "bazel 50",
        "JavaOptions; the tools-side twin of --java_language_version, same reasoning.",
    ),
    e(
        "cxxopt",
        Effect::Irrelevant,
        "apollo, two steps (`build --cxxopt` in tools/bazel.rc, reached by import)",
        "CppOptions; help tags action_command_lines, affects_outputs; not accepted by `bazel \
         query`. Compile command lines only. (Under `build`, which never reaches a query in \
         any case.)",
    ),
    e(
        "test_tag_filters",
        Effect::Irrelevant,
        "envoy 51 (cleared: already allowlisted)",
        "LoadingOptions, but included only by the build and print_action commands (test \
         inherits build); absent from `help query --long`. Filters which already-loaded test \
         targets `bazel test` selects; no package content or edge changes. Also config-scoped \
         in every instance.",
    ),
    e(
        "repo_env",
        Effect::Relevant,
        "xla 239 (`common:cuda_version --repo_env=HERMETIC_NCCL_VERSION`)",
        "CommonCommandOptions, so every command including query reads it (the help tag \
         action_command_lines is misleading). CommandEnvironment folds each --repo_env into \
         the repository environment that repository_ctx.os.environ / getenv and module \
         extensions observe; a changed value re-runs the repository rule and can rewrite the \
         external repository's generated BUILD files. HERMETIC_NCCL_VERSION picks which NCCL \
         redistribution xla's hermetic repository rule downloads. Loading-relevant. The same \
         holds for --action_env=NAME=VALUE (the explicit-value form only): CommandEnvironment \
         copies it into the repository environment for every command whose build phase \
         loads, query included, unless --repo_env_ignores_action_env. Neither is \
         allowlisted.",
    ),
    e(
        "config",
        Effect::Routed,
        "xla 50 and 208 (`--config=rocm`, `--config=workspace` under common:rocm_ci*)",
        "Not a flag but an rc expansion: 'for every <command>, it also pulls in the options \
         from <command>:<config>'. Its loading relevance is exactly that of the expanded \
         section (xla's `common:workspace --noenable_bzlmod --enable_workspace` makes \
         --config=workspace the bzlmod switch, and common:rocm chains through \
         rocm_clang_hermetic to it). The screen expands --config=X into the common:X / \
         always:X / query:X sections at each end, recursively, and classifies the flags \
         those carry, so the reason names `common:X --flag` rather than a bare `--config`. \
         A `--config=X` that no rc file defines for any command is unknown. Both xla \
         instances sit under configs the index query never selects, so they clear.",
    ),
    e(
        "try-import",
        Effect::Routed,
        "apollo 8 (`try-import %workspace%/.custom.bazelrc` added; the file is tracked at \
         neither revision), envoy's bazel/tests/external/.bazelrc (no longer an rc file)",
        "Not a flag: an import edit is loading-relevant exactly when the imported file's \
         triples are. `try-import` is textual inclusion that Bazel skips when the file is \
         missing, so a target absent at both ends contributes no line at either and the \
         edit is harmless; a target present at an end is read into that end's chain and \
         screened like the root. A plain `import` of a missing file fails Bazel, and a \
         target outside the repository cannot be read, so either is unknown when the line \
         itself moved. The engine reads rc text at both revisions from git, never from the \
         working tree, so an untracked developer override is out of scope (CI checkouts are \
         clean).",
    ),
    e(
        "@repo//pkg:setting (Starlark build settings)",
        Effect::Routed,
        "envoy 80, 235, 293, 295, 296 (`@quiche//:*`, `@grpc//third_party:*`, `@protobuf//:*`, \
         `@envoy//bazel:{crypto,ssl,enable_bzlmod}`, ...); xla 233 \
         (`@rules_ml_toolchain//common:enable_xla_test_global_symbol_version`)",
        "string_flag / bool_flag / label_flag targets. A build setting's value is \
         configuration, visible only at analysis (ctx.build_setting_value, config_setting \
         flag_values, a label_flag's chosen actual); no BUILD file, macro, repository rule \
         or module extension can read it, so no package's content or edges depend on it. \
         Not loading-relevant by value. Handed to the flag resolver rather than allowlisted: \
         a main-repository setting is seeded through its config_setting edges, which is a \
         better answer than nothing; an external one stays global (W015), and soundly so, \
         because `bazel query` parses Starlark options for every command and loads the \
         flag's package to validate it, so a renamed or removed external setting changes \
         which packages load and can fail the query. Under a config that is not in effect \
         the triple is skipped like any other.",
    ),
];

fn pattern_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

/// Whether a flag name (no leading dashes) is loading-irrelevant.
///
/// A boolean flag can be written `--foo` or `--nofoo`; the negation is the same
/// flag, so `nofoo` is retried as `foo` when the literal name misses.
fn is_allowed(name: &str, extra: &[String]) -> bool {
    let hit = |n: &str| {
        ALLOWLIST.iter().any(|e| pattern_matches(e.pattern, n))
            || extra
                .iter()
                .any(|p| pattern_matches(p.trim_start_matches('-'), n))
    };
    hit(name) || name.strip_prefix("no").is_some_and(hit)
}

/// A flag as it appears in one rc file: which command it applies to, under
/// which `--config` name, and its own name with the dashes stripped.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FlagKey {
    pub command: String,
    pub config: Option<String>,
    pub name: String,
}

/// A parsed bazelrc: flag values keyed by triple, plus the files it imports.
///
/// Values are sorted rather than kept in order, so that moving a line without
/// changing what it says is not a change. Repeats are kept, because
/// `--copt=-O2 --copt=-O2` is not `--copt=-O2` to Bazel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bazelrc {
    pub flags: BTreeMap<FlagKey, Vec<String>>,
    /// `import` and `try-import` targets, with the keyword.
    pub imports: BTreeSet<String>,
}

/// Parses a bazelrc's text.
///
/// Comments (`#` to end of line), blank lines, backslash line continuations,
/// `command:config` prefixes, `import`/`try-import`, and single- or
/// double-quoted tokens. Nothing here fails: a line the parser does not
/// understand becomes a flag with an unrecognisable name, which is unknown, and
/// unknown is global.
pub fn parse(text: &str) -> Bazelrc {
    let mut rc = Bazelrc::default();
    for line in logical_lines(text) {
        let tokens = tokenize(&line);
        let Some((head, rest)) = tokens.split_first() else {
            continue;
        };
        if head == "import" || head == "try-import" {
            for t in rest {
                rc.imports.insert(format!("{head} {t}"));
            }
            continue;
        }
        let (command, config) = match head.split_once(':') {
            Some((c, cfg)) => (c.to_string(), Some(cfg.to_string())),
            None => (head.clone(), None),
        };
        let mut i = 0;
        while i < rest.len() {
            let tok = &rest[i];
            i += 1;
            let (name, value) = match tok.split_once('=') {
                Some((n, v)) => (n.to_string(), v.to_string()),
                None => {
                    // `--jobs 8`: a following token that is not itself a flag
                    // is this flag's value. A positional that follows nothing
                    // keeps itself as its value, so it still counts as changed
                    // when it moves.
                    let v = match rest.get(i) {
                        Some(next) if !next.starts_with('-') && tok.starts_with('-') => {
                            i += 1;
                            next.clone()
                        }
                        _ => String::new(),
                    };
                    (tok.clone(), v)
                }
            };
            let key = FlagKey {
                command: command.clone(),
                config: config.clone(),
                name: name.trim_start_matches('-').to_string(),
            };
            rc.flags.entry(key).or_default().push(value);
        }
    }
    for values in rc.flags.values_mut() {
        values.sort();
    }
    rc
}

/// Joins backslash continuations and strips comments, yielding one string per
/// logical line.
fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending = String::new();
    for raw in text.lines() {
        let line = strip_comment(raw);
        match line.strip_suffix('\\') {
            Some(head) => {
                pending.push_str(head);
                pending.push(' ');
            }
            None => {
                pending.push_str(&line);
                if !pending.trim().is_empty() {
                    out.push(std::mem::take(&mut pending));
                } else {
                    pending.clear();
                }
            }
        }
    }
    if !pending.trim().is_empty() {
        out.push(pending);
    }
    out
}

/// Everything before an unquoted `#` that starts a token.
fn strip_comment(line: &str) -> String {
    let mut quote: Option<char> = None;
    let mut prev_ws = true;
    for (i, ch) in line.char_indices() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == '#' && prev_ws => return line[..i].trim_end().to_string(),
            None => {}
        }
        prev_ws = ch.is_whitespace();
    }
    line.trim_end().to_string()
}

/// Whitespace-separated tokens, honouring single and double quotes.
fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for ch in line.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                started = true;
            }
            None if ch.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            None => {
                cur.push(ch);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// The triples whose values differ between two parsed files, in a stable order.
pub fn changed_flags(base: &Bazelrc, head: &Bazelrc) -> Vec<FlagKey> {
    let mut keys: BTreeSet<&FlagKey> = base.flags.keys().collect();
    keys.extend(head.flags.keys());
    keys.into_iter()
        .filter(|k| base.flags.get(*k) != head.flags.get(*k))
        .cloned()
        .collect()
}

/// The label a flag name denotes, if it is one.
///
/// `@repo//pkg:name`, `@@canonical//pkg:name` and `//pkg:name` are Starlark
/// build settings; everything else (`define`, `jobs`, `platforms`) is a native
/// flag and has no target behind it. A boolean setting is negated by writing
/// `--no//pkg:name`, so a leading `no` in front of a label is stripped, and
/// only in front of a label, so `nostamp` stays `nostamp`.
///
/// `name` has already had its leading dashes removed by [`parse`].
pub fn flag_label(name: &str) -> Option<&str> {
    let label = match name.strip_prefix("no") {
        Some(rest) if rest.starts_with('@') || rest.starts_with("//") => rest,
        _ => name,
    };
    // Syntactic only: whether the name has the shape of a label, not where
    // it points. The repository is not resolved here ([`crate::flags`] does
    // that, with the mapping), so `@repo//pkg:name` is a label whatever
    // `repo` is, as long as it is a name and something follows the `//`.
    let l = Label::parse(label)?;
    match l.repo() {
        Repo::Main => (!l.is_relative()).then_some(label),
        Repo::Apparent(repo) | Repo::Canonical(repo) => {
            (!repo.is_empty() && !repo.contains(':') && !l.rest().is_empty()).then_some(label)
        }
    }
}

/// A boolean flag's value as an rc file writes it: bare (`--foo`), or with
/// one of the spellings Bazel accepts.
fn truthy(values: &[String]) -> bool {
    values
        .iter()
        .any(|v| matches!(v.as_str(), "" | "true" | "1" | "yes"))
}

/// The triples of one end of the range that a `bazel query` reads, with the
/// configs that were in effect to get there.
#[derive(Debug, Default)]
struct Effective {
    /// Triple -> its values from every file that sets it, sorted.
    flags: BTreeMap<FlagKey, Vec<String>>,
    /// Triple -> the first file (in path order) that sets it.
    source: BTreeMap<FlagKey, String>,
    /// Startup options, which every command reads: name -> values.
    startup: BTreeMap<String, Vec<String>>,
    startup_source: BTreeMap<String, String>,
    /// `--config=X` lines in effect whose `X` no file defines for any
    /// command: the line's scope and the name.
    undefined: BTreeSet<(FlagKey, String)>,
}

/// Whether a triple reaches the query given the configs in effect.
fn in_effect(key: &FlagKey, configs: &BTreeSet<String>) -> bool {
    QUERY_COMMANDS.contains(&key.command.as_str())
        && key.config.as_ref().is_none_or(|c| configs.contains(c))
}

fn effective(chain: &Chain, inv: &Invocation) -> Effective {
    // The configs in effect for the query: what the invocation passes, plus
    // every `--config=` a line in effect names, to a fixpoint, plus the
    // platform names once `--enable_platform_specific_config` is in effect.
    // A negated `--noenable_platform_specific_config` beside a positive one
    // is read as on: order is not kept, and on screens more, never less.
    let mut configs: BTreeSet<String> = inv.configs.iter().cloned().collect();
    loop {
        let mut grew = false;
        for rc in chain.files.values() {
            for (key, values) in &rc.flags {
                if !in_effect(key, &configs) {
                    continue;
                }
                match key.name.as_str() {
                    "config" => {
                        for v in values {
                            grew |= configs.insert(v.clone());
                        }
                    }
                    "enable_platform_specific_config" if truthy(values) => {
                        for p in PLATFORM_CONFIGS {
                            grew |= configs.insert((*p).to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
        if !grew {
            break;
        }
    }

    let defined: BTreeSet<&str> = chain
        .files
        .values()
        .flat_map(|rc| rc.flags.keys())
        .filter_map(|k| k.config.as_deref())
        .collect();

    let mut out = Effective::default();
    for (path, rc) in &chain.files {
        for (key, values) in &rc.flags {
            if key.command == "startup" {
                out.startup
                    .entry(key.name.clone())
                    .or_default()
                    .extend(values.iter().cloned());
                out.startup_source
                    .entry(key.name.clone())
                    .or_insert_with(|| path.clone());
                continue;
            }
            if !in_effect(key, &configs) {
                continue;
            }
            match key.name.as_str() {
                // Expanded above; the flags they pull in are what is screened.
                "enable_platform_specific_config" | "noenable_platform_specific_config" => continue,
                "config" => {
                    for v in values {
                        if !defined.contains(v.as_str()) && !PLATFORM_CONFIGS.contains(&v.as_str())
                        {
                            out.undefined.insert((key.clone(), v.clone()));
                        }
                    }
                    // Not a flag of its own, but the reason for an undefined
                    // config names the file that carries the line.
                    out.source
                        .entry(key.clone())
                        .or_insert_with(|| path.clone());
                    continue;
                }
                _ => {}
            }
            out.flags
                .entry(key.clone())
                .or_default()
                .extend(values.iter().cloned());
            out.source
                .entry(key.clone())
                .or_insert_with(|| path.clone());
        }
    }
    for v in out.flags.values_mut().chain(out.startup.values_mut()) {
        v.sort();
    }
    out
}

/// Reads the rc chains at the two ends of a range into the things a run can
/// act on: the count of triples the query reads that changed, the
/// label-valued Starlark flags among them, whether `--registry` moved, and
/// the first change that was none of those and not allowlisted.
///
/// Returns on the first unknown; the other fields are then partial and the
/// caller does not read them.
pub fn screen_chain(base: &Chain, head: &Chain, inv: &Invocation) -> Screen {
    let mut out = Screen::default();
    let blocked = |path: &str, why: Unknown| Screen {
        unknown: Some(Blocker {
            path: path.to_string(),
            why,
        }),
        ..Screen::default()
    };

    // An import line that moved and whose target cannot be read at the end
    // it applies to. One present at both ends is a fact about the
    // environment, not about this range.
    let mut imports: Vec<(&String, &String)> = base
        .unresolved
        .iter()
        .filter(|(line, _)| !head.unresolved.contains_key(*line))
        .chain(
            head.unresolved
                .iter()
                .filter(|(line, _)| !base.unresolved.contains_key(*line)),
        )
        .collect();
    imports.sort();
    if let Some((line, path)) = imports.first() {
        return blocked(
            path,
            Unknown::ImportChanged {
                import: (*line).clone(),
            },
        );
    }

    let (b, h) = (effective(base, inv), effective(head, inv));

    let mut startup: BTreeSet<&String> = b.startup.keys().collect();
    startup.extend(h.startup.keys());
    for name in startup {
        if b.startup.get(name) != h.startup.get(name) {
            let path = h
                .startup_source
                .get(name)
                .or_else(|| b.startup_source.get(name))
                .cloned()
                .unwrap_or_default();
            return blocked(&path, Unknown::StartupFlag { name: name.clone() });
        }
    }

    if let Some((key, config)) = b.undefined.symmetric_difference(&h.undefined).next() {
        let path = h
            .source
            .get(key)
            .or_else(|| b.source.get(key))
            .cloned()
            .unwrap_or_default();
        return blocked(
            &path,
            Unknown::ConfigUndefined {
                command: scope(&key.command, key.config.as_deref()),
                config: config.clone(),
            },
        );
    }

    let mut keys: BTreeSet<&FlagKey> = b.flags.keys().collect();
    keys.extend(h.flags.keys());
    let changed: Vec<&FlagKey> = keys
        .into_iter()
        .filter(|k| b.flags.get(*k) != h.flags.get(*k))
        .collect();
    out.changed_flags = changed.len();
    for key in changed {
        let path = h
            .source
            .get(key)
            .or_else(|| b.source.get(key))
            .cloned()
            .unwrap_or_default();
        if is_allowed(&key.name, inv.extra_allowlist) {
            continue;
        }
        if key.name == "registry" {
            out.registry.get_or_insert(RegistryChange {
                path,
                key: key.clone(),
            });
            continue;
        }
        match flag_label(&key.name) {
            Some(label) => out.label_flags.push(LabelFlag {
                key: key.clone(),
                label: label.to_string(),
            }),
            None => {
                return blocked(
                    &path,
                    Unknown::NotAllowlisted {
                        command: key.command.clone(),
                        config: key.config.clone(),
                        name: key.name.clone(),
                    },
                );
            }
        }
    }
    out
}

/// A chain of one file, `.bazelrc`, with nothing to resolve its imports
/// against: every import line is unresolved, so any that moves is unknown.
fn lone(text: &str) -> Chain {
    let rc = parse(text);
    let unresolved = rc
        .imports
        .iter()
        .map(|i| (i.clone(), ".bazelrc".to_string()))
        .collect();
    Chain {
        files: [(".bazelrc".to_string(), rc)].into_iter().collect(),
        unresolved,
    }
}

/// [`screen_chain`] over one file's two texts, with no configs passed on the
/// command line and no other rc file to resolve imports against.
pub fn screen(base_text: &str, head_text: &str, extra_allowlist: &[String]) -> Screen {
    screen_chain(
        &lone(base_text),
        &lone(head_text),
        &Invocation {
            configs: &[],
            extra_allowlist,
        },
    )
}

/// Classifies a bazelrc edit without an index to resolve label flags against
/// and without the externals stage to answer a registry change: safe only if
/// every changed triple the query reads is allowlisted and the imports are
/// untouched. A Starlark flag or `--registry` is unknown here;
/// [`screen_chain`] is what the engine uses, and it can do better.
///
/// See [`Verdict`] for why this survives.
#[doc(hidden)]
pub fn classify(base_text: &str, head_text: &str, extra_allowlist: &[String]) -> Verdict {
    let s = screen(base_text, head_text, extra_allowlist);
    if let Some(b) = s.unknown {
        return Verdict::Unknown(b.why.to_string());
    }
    if let Some(r) = s.registry {
        return Verdict::Unknown(format!(
            "{} --registry changed; a lock-file question the externals stage answers",
            scope(&r.key.command, r.key.config.as_deref())
        ));
    }
    if let Some(f) = s.label_flags.first() {
        return Verdict::Unknown(
            Unknown::NotAllowlisted {
                command: f.key.command.clone(),
                config: f.key.config.clone(),
                name: f.key.name.clone(),
            }
            .to_string(),
        );
    }
    Verdict::Safe {
        changed_flags: s.changed_flags,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(command: &str, config: Option<&str>, name: &str) -> FlagKey {
        FlagKey {
            command: command.into(),
            config: config.map(String::from),
            name: name.into(),
        }
    }

    #[test]
    fn parses_commands_configs_and_comments() {
        let rc = parse(
            "# a comment\n\
             build --jobs=8   # trailing comment\n\
             build:ci --remote_cache=grpc://x\n\
             \n\
             test --test_output=errors\n",
        );
        assert_eq!(rc.flags[&key("build", None, "jobs")], vec!["8"]);
        assert_eq!(
            rc.flags[&key("build", Some("ci"), "remote_cache")],
            vec!["grpc://x"]
        );
        assert_eq!(rc.flags[&key("test", None, "test_output")], vec!["errors"]);
        assert!(rc.imports.is_empty());
    }

    #[test]
    fn joins_line_continuations() {
        let rc = parse("build --jobs=8 \\\n  --remote_cache=x \\\n  --color=yes\n");
        assert_eq!(rc.flags[&key("build", None, "color")], vec!["yes"]);
        assert_eq!(rc.flags[&key("build", None, "remote_cache")], vec!["x"]);
    }

    #[test]
    fn separate_value_tokens_and_quotes() {
        let rc = parse("build --jobs 8 --build_metadata=\"a b\"\n");
        assert_eq!(rc.flags[&key("build", None, "jobs")], vec!["8"]);
        assert_eq!(rc.flags[&key("build", None, "build_metadata")], vec!["a b"]);
    }

    #[test]
    fn imports_are_recorded() {
        let rc = parse("import %workspace%/other.bazelrc\ntry-import %workspace%/user.bazelrc\n");
        assert_eq!(rc.imports.len(), 2);
        assert!(rc.imports.iter().any(|i| i.starts_with("try-import ")));
    }

    #[test]
    fn reordering_and_whitespace_are_not_changes() {
        let a = "common --jobs=8\ncommon --color=yes\n";
        let b = "\n\ncommon   --color=yes\n# note\ncommon --jobs=8\n";
        assert!(changed_flags(&parse(a), &parse(b)).is_empty());
        assert_eq!(classify(a, b, &[]), Verdict::Safe { changed_flags: 0 });
    }

    #[test]
    fn added_removed_and_changed_values_are_all_changes() {
        let base = "build --jobs=8\nbuild --color=yes\n";
        let head = "build --jobs=16\nbuild --test_output=errors\n";
        let changed = changed_flags(&parse(base), &parse(head));
        let names: Vec<&str> = changed.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(names, vec!["color", "jobs", "test_output"]);
    }

    #[test]
    fn a_config_is_part_of_the_key() {
        let changed = changed_flags(
            &parse("build --jobs=8\n"),
            &parse("build --jobs=8\nbuild:ci --jobs=8\n"),
        );
        assert_eq!(changed, vec![key("build", Some("ci"), "jobs")]);
    }

    #[test]
    fn allowlisted_edits_are_safe() {
        let base =
            "common --remote_cache=grpc://old\ncommon --jobs=8\nquery --test_output=summary\n";
        let head =
            "common --remote_cache=grpc://new\ncommon --jobs=16\nquery --test_output=errors\n\
                    common --bes_backend=grpc://bes\nalways --noshow_progress\n";
        assert_eq!(
            classify(base, head, &[]),
            Verdict::Safe { changed_flags: 5 }
        );
    }

    #[test]
    fn test_selection_filters_are_safe() {
        let base = "common --test_tag_filters=-flaky\n";
        let head = "common --test_tag_filters=-flaky,-slow\ncommon --test_size_filters=small\n";
        assert_eq!(
            classify(base, head, &[]),
            Verdict::Safe { changed_flags: 2 }
        );
    }

    /// The flags the subject histories fired on, now allowlisted: analysis
    /// configuration that `bazel query` never consults.
    #[test]
    fn analysis_only_flags_from_the_evidence_are_safe() {
        let base = "common --jobs=8\n";
        let head = "common --jobs=8\ncommon --define=x=1\ncommon --features=-foo\n\
                    common --host_features=-foo\ncommon --per_file_copt=x@-O2\n\
                    common --java_language_version=25\ncommon --tool_java_language_version=25\n\
                    common --dynamic_mode=off\ncommon --incompatible_default_to_explicit_init_py\n\
                    common --cxxopt=-std=c++20\n";
        assert_eq!(
            classify(base, head, &[]),
            Verdict::Safe { changed_flags: 9 }
        );
    }

    #[test]
    fn a_repo_env_is_unknown() {
        let v = classify(
            "common --jobs=8\n",
            "common --jobs=8\ncommon --repo_env=CC=clang\n",
            &[],
        );
        assert_eq!(
            v,
            Verdict::Unknown(
                "common --repo_env changed, not on the loading-irrelevant allowlist".into()
            )
        );
    }

    #[test]
    fn a_starlark_flag_and_a_graph_flag_are_unknown() {
        for line in [
            "common --//flags:mode=fast",
            "common --@rules_go//go/config:race",
            "common --config=ci",
            "common --platforms=//p:linux",
            "common --enable_bzlmod",
            "common --override_repository=foo=/tmp/foo",
            "common --incompatible_enable_proto_toolchain_resolution",
            "common --action_env=FOO=1",
            "query --deleted_packages=a/b",
        ] {
            let v = classify(
                "common --jobs=8\n",
                &format!("common --jobs=8\n{line}\n"),
                &[],
            );
            assert!(matches!(v, Verdict::Unknown(_)), "{line} classified {v:?}");
        }
    }

    /// `query` inherits from no command, so a line under `build`, `test` or
    /// any other command cannot change what the index query sees.
    #[test]
    fn a_line_the_query_does_not_read_is_not_a_change() {
        for line in [
            "build --repo_env=CC=clang",
            "test --enable_bzlmod",
            "cquery --platforms=//p:linux",
            "coverage --config=coverage",
            "build:ci --//flags:mode=fast",
            "run --deleted_packages=a/b",
        ] {
            let v = classify(
                "common --jobs=8\n",
                &format!("common --jobs=8\n{line}\n"),
                &[],
            );
            assert_eq!(v, Verdict::Safe { changed_flags: 0 }, "{line}");
        }
    }

    /// A triple under `command:config` reaches the query only when that
    /// config is in effect for it; the ones the chain never names are skipped.
    #[test]
    fn a_config_the_query_does_not_run_with_is_skipped() {
        let v = classify(
            "common --jobs=8\n",
            "common --jobs=8\ncommon:ci --repo_env=CC=clang\nquery:ci --enable_bzlmod\n",
            &[],
        );
        assert_eq!(v, Verdict::Safe { changed_flags: 0 });
    }

    #[test]
    fn a_config_the_chain_names_is_in_effect_and_the_reason_names_it() {
        let base = "common --config=ci\ncommon:ci --jobs=8\n";
        let head = "common --config=ci\ncommon:ci --jobs=8\ncommon:ci --repo_env=CC=clang\n";
        assert_eq!(
            classify(base, head, &[]),
            Verdict::Unknown(
                "common:ci --repo_env changed, not on the loading-irrelevant allowlist".into()
            )
        );
        // Named through a chain of configs, each in effect through the last.
        let base = "common --config=ci\ncommon:ci --config=remote\ncommon:remote --jobs=8\n";
        let head = "common --config=ci\ncommon:ci --config=remote\ncommon:remote --jobs=8\n\
                    common:remote --repo_env=CC=clang\n";
        assert!(matches!(
            classify(base, head, &[]),
            Verdict::Unknown(m) if m.starts_with("common:remote --repo_env")
        ));
        // Named only under a command the query does not read: not in effect.
        let base = "build --config=ci\ncommon:ci --jobs=8\n";
        let head = "build --config=ci\ncommon:ci --jobs=8\ncommon:ci --repo_env=CC=clang\n";
        assert_eq!(
            classify(base, head, &[]),
            Verdict::Safe { changed_flags: 0 }
        );
    }

    #[test]
    fn the_invocation_passes_configs_too() {
        let base = "common:ci --jobs=8\n";
        let head = "common:ci --jobs=8\ncommon:ci --repo_env=CC=clang\n";
        assert_eq!(
            classify(base, head, &[]),
            Verdict::Safe { changed_flags: 0 }
        );
        let ci = ["ci".to_string()];
        let s = screen_chain(
            &lone(base),
            &lone(head),
            &Invocation {
                configs: &ci,
                extra_allowlist: &[],
            },
        );
        assert!(
            matches!(&s.unknown, Some(b) if b.why.to_string().starts_with("common:ci --repo_env")),
            "{s:?}"
        );
    }

    /// Adding or removing a `--config=` line is screened as the flags the
    /// section carries, so a section of harmless flags is harmless and one
    /// with a graph flag names that flag.
    #[test]
    fn a_config_reference_is_expanded_into_its_section() {
        let base = "common:ci --jobs=8\ncommon:ci --remote_cache=x\n";
        let head = "common --config=ci\ncommon:ci --jobs=8\ncommon:ci --remote_cache=x\n";
        assert_eq!(
            classify(base, head, &[]),
            Verdict::Safe { changed_flags: 2 }
        );
        let base = "common:ci --repo_env=CC=clang\n";
        let head = "common --config=ci\ncommon:ci --repo_env=CC=clang\n";
        assert!(matches!(
            classify(base, head, &[]),
            Verdict::Unknown(m) if m.starts_with("common:ci --repo_env")
        ));
        // A section defined only for a command the query does not read
        // expands to nothing for the query.
        let base = "build:ci --repo_env=CC=clang\n";
        let head = "common --config=ci\nbuild:ci --repo_env=CC=clang\n";
        assert_eq!(
            classify(base, head, &[]),
            Verdict::Safe { changed_flags: 0 }
        );
    }

    #[test]
    fn a_config_no_file_defines_is_unknown() {
        let v = classify(
            "common --jobs=8\n",
            "common --jobs=8\ncommon --config=ci\n",
            &[],
        );
        assert_eq!(
            v,
            Verdict::Unknown(
                "common --config=ci changed, and no rc file defines `ci` for any command".into()
            )
        );
        // Pre-existing at both ends: not this range's problem.
        assert_eq!(
            classify(
                "common --config=ci\n",
                "common --config=ci\ncommon --jobs=8\n",
                &[]
            ),
            Verdict::Safe { changed_flags: 1 }
        );
    }

    #[test]
    fn platform_configs_are_in_effect_when_the_flag_is() {
        let base = "common:linux --jobs=8\n";
        let head = "common:linux --jobs=8\ncommon:linux --repo_env=CC=clang\n";
        assert_eq!(
            classify(base, head, &[]),
            Verdict::Safe { changed_flags: 0 }
        );
        let on = |rc: &str| format!("common --enable_platform_specific_config\n{rc}");
        assert!(matches!(
            classify(&on(base), &on(head), &[]),
            Verdict::Unknown(m) if m.starts_with("common:linux --repo_env")
        ));
        // Every host's section counts, since the host is not modelled.
        let head = "common:openbsd --repo_env=CC=clang\n";
        assert!(matches!(
            classify(&on(""), &on(head), &[]),
            Verdict::Unknown(m) if m.starts_with("common:openbsd --repo_env")
        ));
        // Under `build` the flag does not reach the query.
        let build_on = |rc: &str| format!("build --enable_platform_specific_config\n{rc}");
        assert_eq!(
            classify(&build_on(base), &build_on(head), &[]),
            Verdict::Safe { changed_flags: 0 }
        );
        // An undefined platform section is what Bazel skips too.
        assert_eq!(
            classify(&on(""), &on("common --jobs=8\n"), &[]),
            Verdict::Safe { changed_flags: 1 }
        );
    }

    #[test]
    fn a_startup_line_is_unknown_even_when_the_flag_looks_harmless() {
        let v = classify(
            "startup --max_idle_secs=10\n",
            "startup --max_idle_secs=20\n",
            &[],
        );
        assert!(
            matches!(&v, Verdict::Unknown(m) if m.contains("startup")),
            "{v:?}"
        );
    }

    #[test]
    fn an_added_import_is_unknown_without_a_chain_to_resolve_it() {
        let v = classify(
            "build --jobs=8\n",
            "build --jobs=8\nimport %workspace%/ci.bazelrc\n",
            &[],
        );
        assert!(
            matches!(&v, Verdict::Unknown(m) if m.contains("import")),
            "{v:?}"
        );
    }

    fn files(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(p, t)| (p.to_string(), t.to_string()))
            .collect()
    }

    fn chain_of(files: &BTreeMap<String, String>) -> Chain {
        let read = |p: &str| files.get(p).cloned();
        chain(std::path::Path::new("/repo"), &[], &read)
    }

    fn screen_files(base: &[(&str, &str)], head: &[(&str, &str)]) -> Screen {
        screen_chain(
            &chain_of(&files(base)),
            &chain_of(&files(head)),
            &Invocation::default(),
        )
    }

    /// The chain is followed at each end: an import line that moved is
    /// screened as the flags of the file it names.
    #[test]
    fn an_import_is_resolved_against_the_chain() {
        // try-import of a file present at neither end: harmless.
        let s = screen_files(
            &[(".bazelrc", "common --jobs=8\n")],
            &[(
                ".bazelrc",
                "common --jobs=8\ntry-import %workspace%/.user.bazelrc\n",
            )],
        );
        assert_eq!(s.unknown, None, "{s:?}");
        assert_eq!(s.changed_flags, 0);
        // A plain import of a missing file would fail Bazel: unknown.
        let s = screen_files(
            &[(".bazelrc", "common --jobs=8\n")],
            &[(
                ".bazelrc",
                "common --jobs=8\nimport %workspace%/ci.bazelrc\n",
            )],
        );
        assert!(
            matches!(&s.unknown, Some(b) if matches!(b.why, Unknown::ImportChanged { .. })),
            "{s:?}"
        );
        // One outside the repository cannot be read: unknown.
        let s = screen_files(
            &[(".bazelrc", "common --jobs=8\n")],
            &[(
                ".bazelrc",
                "common --jobs=8\ntry-import /etc/bazel.bazelrc\n",
            )],
        );
        assert!(
            matches!(&s.unknown, Some(b) if b.path == ".bazelrc" && matches!(b.why, Unknown::ImportChanged { .. })),
            "{s:?}"
        );
        // ... unless it was there at both ends.
        let s = screen_files(
            &[(".bazelrc", "try-import /etc/bazel.bazelrc\n")],
            &[(
                ".bazelrc",
                "try-import /etc/bazel.bazelrc\ncommon --jobs=8\n",
            )],
        );
        assert_eq!(s.unknown, None, "{s:?}");
        // A file present at both ends whose import line was added: its
        // flags newly apply, and the reason names that file.
        let ci = (
            "ci.bazelrc",
            "common --jobs=4\ncommon --repo_env=CC=clang\n",
        );
        let s = screen_files(
            &[(".bazelrc", "common --jobs=8\n"), ci],
            &[
                (
                    ".bazelrc",
                    "common --jobs=8\nimport %workspace%/ci.bazelrc\n",
                ),
                ci,
            ],
        );
        assert_eq!(
            s.unknown,
            Some(Blocker {
                path: "ci.bazelrc".into(),
                why: Unknown::NotAllowlisted {
                    command: "common".into(),
                    config: None,
                    name: "repo_env".into(),
                },
            })
        );
        let ci = (
            "ci.bazelrc",
            "common --jobs=4\nimport %workspace%/tools/x.rc\n",
        );
        let x = ("tools/x.rc", "common --remote_cache=grpc://x\n");
        let s = screen_files(
            &[(".bazelrc", "common --jobs=8\n"), ci, x],
            &[
                (
                    ".bazelrc",
                    "common --jobs=8\nimport %workspace%/ci.bazelrc\n",
                ),
                ci,
                x,
            ],
        );
        assert_eq!(s.unknown, None, "{s:?}");
        assert_eq!(s.changed_flags, 2, "jobs and the nested remote_cache");
    }

    #[test]
    fn a_registry_change_is_reported_not_unknown() {
        let s = screen(
            "common --registry=https://bcr.bazel.build/\n",
            "common --registry=https://bcr.bazel.build/\ncommon --registry=https://example.com/r\n",
            &[],
        );
        assert_eq!(s.unknown, None);
        assert_eq!(
            s.registry,
            Some(RegistryChange {
                path: ".bazelrc".into(),
                key: key("common", None, "registry"),
            })
        );
        assert!(matches!(
            classify("", "common --registry=https://example.com/r\n", &[]),
            Verdict::Unknown(m) if m.contains("lock-file")
        ));
        // Under `build` it never reaches the query.
        let s = screen("", "build --registry=https://example.com/r\n", &[]);
        assert_eq!(s.registry, None);
    }

    #[test]
    fn the_extra_allowlist_extends_it() {
        let base = "common --my_local_flag=1\n";
        let head = "common --my_local_flag=2\n";
        assert!(matches!(classify(base, head, &[]), Verdict::Unknown(_)));
        assert_eq!(
            classify(base, head, &["--my_local_*".to_string()]),
            Verdict::Safe { changed_flags: 1 }
        );
    }

    #[test]
    fn negated_booleans_match_their_positive_form() {
        assert!(is_allowed("nocache_test_results", &[]));
        assert!(!is_allowed("norepo_env", &[]));
    }

    /// The evidence table and the allowlist say the same thing about every
    /// flag that fired.
    #[test]
    fn the_allowlist_agrees_with_the_evidence() {
        for ev in EVIDENCE {
            let allowed = is_allowed(ev.flag, &[]);
            match ev.verdict {
                Effect::Irrelevant => assert!(allowed, "{} should be allowlisted", ev.flag),
                Effect::Relevant => assert!(!allowed, "{} must not be allowlisted", ev.flag),
                Effect::Routed => assert!(!allowed, "{} is routed, not allowlisted", ev.flag),
            }
            assert!(!ev.why.is_empty() && !ev.fired_on.is_empty(), "{}", ev.flag);
        }
    }

    #[test]
    fn label_flags_are_recognised_in_every_spelling() {
        assert_eq!(flag_label("@repo//x:y"), Some("@repo//x:y"));
        assert_eq!(flag_label("@@repo+//x:y"), Some("@@repo+//x:y"));
        assert_eq!(flag_label("//x:y"), Some("//x:y"));
        assert_eq!(flag_label("//x/y:z"), Some("//x/y:z"));
        // A package with no target part is still a label.
        assert_eq!(flag_label("@quiche//:ssl_lib"), Some("@quiche//:ssl_lib"));
        // Boolean negation of a setting, and a native flag that starts `no`.
        assert_eq!(flag_label("no//x:y"), Some("//x:y"));
        assert_eq!(flag_label("no@repo//x:y"), Some("@repo//x:y"));
        assert_eq!(flag_label("nostamp"), None);
        assert_eq!(flag_label("nocache_test_results"), None);
        for native in ["define", "jobs", "platforms", "config", "", "@", "@repo"] {
            assert_eq!(flag_label(native), None, "{native}");
        }
    }

    #[test]
    fn a_label_flag_is_handed_back_rather_than_called_unknown() {
        let s = screen(
            "common --jobs=8\n",
            "common --jobs=8\ncommon --@envoy//bazel:crypto=boringssl\nquery --//bazel:foo=bar\n",
            &[],
        );
        assert_eq!(s.unknown, None);
        assert_eq!(s.changed_flags, 2);
        let labels: Vec<&str> = s.label_flags.iter().map(|f| f.label.as_str()).collect();
        // Keyed by (command, config, name): `common` sorts before `query`.
        assert_eq!(labels, ["@envoy//bazel:crypto", "//bazel:foo"]);
    }

    #[test]
    fn a_label_flag_under_a_config_follows_the_config() {
        // Not in effect: skipped.
        let s = screen(
            "common --jobs=8\n",
            "common --jobs=8\ncommon:ci --@envoy//bazel:crypto=boringssl\n",
            &[],
        );
        assert_eq!(s.unknown, None, "{s:?}");
        assert!(s.label_flags.is_empty());
        // In effect: handed back with its scope.
        let s = screen(
            "common --config=ci\ncommon:ci --jobs=8\n",
            "common --config=ci\ncommon:ci --jobs=8\ncommon:ci --@envoy//bazel:crypto=boringssl\n",
            &[],
        );
        assert_eq!(s.unknown, None, "{s:?}");
        assert_eq!(
            s.label_flags,
            vec![LabelFlag {
                key: key("common", Some("ci"), "@envoy//bazel:crypto"),
                label: "@envoy//bazel:crypto".into(),
            }]
        );
    }

    #[test]
    fn an_unknown_native_flag_still_wins_over_a_label_flag() {
        let s = screen(
            "common --jobs=8\n",
            "common --jobs=8\ncommon --//bazel:foo=bar\ncommon --repo_env=x=1\n",
            &[],
        );
        assert!(s.unknown.is_some(), "{s:?}");
    }

    #[test]
    fn only_rc_files_the_root_reaches_are_rc_files() {
        let root = std::path::Path::new("/repo");
        let files: BTreeMap<&str, &str> = [
            (".bazelrc", "import %workspace%/ci.bazelrc\ntry-import /repo/user.bazelrc\ntry-import /elsewhere/x.bazelrc\n"),
            ("ci.bazelrc", "import %workspace%/tools/shared.rc\n"),
            ("tools/shared.rc", "build --jobs=8\n"),
            ("tests/fixture/.bazelrc", "build --define=x=1\n"),
        ]
        .into_iter()
        .collect();
        let read = |p: &str| files.get(p).map(|t| t.to_string());
        let flags = vec![
            "--bazelrc=/repo/extra.bazelrc".to_string(),
            "--host_jvm_args=-Xmx1g".to_string(),
        ];
        let got = reachable(root, &flags, &read);
        // `user.bazelrc` is named but absent, so it is not part of the chain
        // at this end; at an end where it exists it would be.
        let want: BTreeSet<String> = [".bazelrc", "ci.bazelrc", "tools/shared.rc"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(got, want);
        assert!(
            !got.contains("tests/fixture/.bazelrc"),
            "a nested workspace's rc is not this workspace's"
        );
        let c = chain(root, &flags, &read);
        assert_eq!(
            c.unresolved.keys().cloned().collect::<Vec<_>>(),
            vec!["try-import /elsewhere/x.bazelrc".to_string()],
            "outside the repository; a missing try-import inside it is silent"
        );
    }

    /// Under `--noworkspace_rc` the root rc and its imports are not read;
    /// a `--bazelrc=` file inside the repository still is.
    #[test]
    fn noworkspace_rc_drops_the_root_rc_and_its_imports() {
        let root = std::path::Path::new("/repo");
        let files: BTreeMap<&str, &str> = [
            (
                ".bazelrc",
                "import %workspace%/ci.bazelrc\ncommon --repo_env=A=1\n",
            ),
            ("ci.bazelrc", "common --jobs=8\n"),
            ("tools/extra.rc", "common --jobs=4\n"),
        ]
        .into_iter()
        .collect();
        let read = |p: &str| files.get(p).map(|t| t.to_string());
        let paths = |flags: &[&str]| {
            let flags: Vec<String> = flags.iter().map(|f| f.to_string()).collect();
            reachable(root, &flags, &read)
        };
        let want =
            |names: &[&str]| -> BTreeSet<String> { names.iter().map(|n| n.to_string()).collect() };
        assert_eq!(
            paths(&["--noworkspace_rc", "--bazelrc=/repo/tools/extra.rc"]),
            want(&["tools/extra.rc"])
        );
        assert_eq!(paths(&["--noworkspace_rc"]), want(&[]));
        // The last spelling wins, as in Bazel's startup parser.
        assert_eq!(
            paths(&["--noworkspace_rc", "--workspace_rc"]),
            want(&[".bazelrc", "ci.bazelrc"])
        );
        assert!(reads_workspace_rc(&[]));
        assert!(!reads_workspace_rc(&[
            "--workspace_rc".to_string(),
            "--noworkspace_rc".to_string()
        ]));
    }
}
