//! Spawning Bazel.
//!
//! Everything here is deliberately thin: two query shapes (a package set and
//! the whole repo), one `rbuildfiles` call, and version detection. The flags
//! are pinned to what the spec's core invariant needs — unconfigured graph,
//! unflattened selects, implicit deps left in — and are the only Bazel surface
//! cambium depends on.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;

pub mod convert;

pub use convert::{convert, ConvertOptions, ParsedTarget};

use cambium_proto::reader::{read_frames, ReadError};

#[derive(Debug, thiserror::Error)]
pub enum BazelError {
    #[error(
        "no Bazel workspace found: walked up from {0} without finding MODULE.bazel or WORKSPACE"
    )]
    NoWorkspace(PathBuf),
    #[error("failed to run {binary}: {source}")]
    Spawn {
        binary: String,
        #[source]
        source: std::io::Error,
    },
    #[error("bazel {args} exited with {status}\n--- last stderr ---\n{stderr}")]
    Failed {
        args: String,
        status: String,
        stderr: String,
    },
    #[error("reading bazel output: {0}")]
    Read(#[from] ReadError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

type Result<T> = std::result::Result<T, BazelError>;

/// How many trailing stderr lines to keep for error reporting (spec 7.4).
const STDERR_TAIL_LINES: usize = 40;

/// Query expressions longer than this go through `--query_file` instead of argv.
const MAX_INLINE_QUERY: usize = 50_000;

/// Flags every invocation gets (spec 7.4).
const HYGIENE: &[&str] = &[
    "--noshow_progress",
    "--noshow_loading_progress",
    "--ui_event_filters=-info",
];

/// Flags that define the graph we index (spec 7.1).
///
/// Note `--proto:flatten_selects=false`, where the spec calls for `true`.
/// (It has to be passed explicitly: Bazel defaults it to true.) Flattening is
/// not needed for the edges — `rule_input` already carries
/// the union over every `select` branch, plus the `config_setting` labels the
/// branches key on — and it actively costs soundness: with it on, an attribute
/// is reported as that same union, so editing one branch to a value another
/// branch already names leaves the rule hash unchanged and the target is
/// silently reported as unaffected. Unflattened, the attribute keeps its
/// per-branch entries, the hash sees the edit, and the edges are identical.
/// (Verified against Bazel 7.4.1; covered by the `change select branch` case in
/// the end-to-end tests.)
///
/// Note also that `--noimplicit_deps` is *not* passed, where it used to be.
///
/// Implicit deps were dropped to keep the toolchain graph — which is external,
/// enormous and never changes — out of the index. It also dropped every
/// implicit edge that points *into the main repository*, and those are real
/// build inputs. sourcegraph's schema generator is the clean example:
///
/// ```text
/// generate_schemas = rule(attrs = {
///     "_sg": attr.label(executable = True, default = "//dev/sg:sg", cfg = "exec"),
/// })
/// ```
///
/// `somepath(//internal/database:generate_schemas, //dev/sg/internal/cloud:cloud)`
/// answers with a three-hop path, and the same query under `--noimplicit_deps`
/// answers "Empty results" — so a commit touching only `dev/sg/internal/cloud`
/// left the generated schemas, and the eleven targets downstream of them,
/// out of cambium's affected set. That is unsoundness, not
/// over-approximation, and no invalidator can name it.
///
/// The cost of keeping them turned out to be nearly nothing, measured on both
/// benchmark subject at its pin: the `//...` proto stream grows 3% on
/// sourcegraph (36.4 -> 37.5 MB) and 4% on envoy (53.5 -> 55.6 MB), the wall
/// time does not move, and neither repository reports a single extra load
/// error. The toolchain labels the flag was there to exclude arrive as
/// external `Unknown` nodes with no outgoing edges: a handful of leaves that
/// never appear in a diff.
const QUERY_FLAGS: &[&str] = &[
    "--output=streamed_proto",
    "--order_output=no",
    "--keep_going",
    "--proto:locations=false",
    "--proto:default_values=false",
    "--proto:flatten_selects=false",
];

/// Result of a query that ran to completion, successfully or not.
#[derive(Debug)]
pub struct QueryOutcome {
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stderr_tail: String,
    pub targets_read: usize,
    /// Packages Bazel reported as failing to load, counted over the *whole*
    /// stderr stream rather than the 40-line tail.
    pub load_errors: LoadErrors,
    /// Packages Bazel said were *declared* deleted by `--deleted_packages`.
    /// Not load failures: see [`parse_deleted_package_line`].
    pub deleted_packages: Vec<String>,
}

/// What `--keep_going` could not load, scraped from stderr.
///
/// Bazel does not report this in the proto stream — a package that failed to
/// load simply is not there — so the only machine-readable trace of an
/// incomplete answer is the `ERROR:` lines. `count` is every such line;
/// `packages` keeps the first [`MAX_ERROR_PACKAGES`] names, which is enough to
/// tell "one broken package" from "the whole repository failed to resolve its
/// external deps" when reading an index's provenance months later.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadErrors {
    pub count: u32,
    pub packages: Vec<String>,
}

impl LoadErrors {
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn push(&mut self, package: &str) {
        self.count += 1;
        if self.packages.len() < MAX_ERROR_PACKAGES {
            self.packages.push(package.to_string());
        }
    }
}

/// How many failing package names an outcome keeps.
pub const MAX_ERROR_PACKAGES: usize = 32;

/// The package named by one Bazel load-error line, if it is one.
///
/// Two shapes, both emitted under `--keep_going`, and both interleaved with the
/// ANSI colour codes Bazel writes even when stderr is a pipe:
///
/// ```text
/// ERROR: error loading package 'modules/planning': at /...: ...
/// ERROR: package contains errors: third_party/bazel: name 'py_library' ...
/// ERROR: no such package '@@local_config_cuda//cuda': ...
/// ```
///
/// The root package comes back as `""`, which is a real package name, so the
/// answer is an `Option` rather than a non-empty string.
pub fn parse_load_error_line(line: &str) -> Option<String> {
    let l = strip_ansi(line);
    // A package the *invocation itself* declared deleted is not a failure to
    // load; see [`parse_deleted_package_line`]. It has to be tested first,
    // because its line carries a `no such package '...'` clause that would
    // otherwise be counted as a load error.
    if deleted_package_of(&l).is_some() {
        return None;
    }
    let rest = l.trim().strip_prefix("ERROR: ")?.trim_start();
    if let Some(r) = rest.strip_prefix("error loading package ") {
        // `'pkg': msg` — quoted, and the message itself may contain quotes.
        let r = r.trim_start().strip_prefix('\'')?;
        let end = r.find('\'')?;
        return Some(r[..end].to_string());
    }
    if let Some(r) = rest.strip_prefix("no such package ") {
        // `'@@repo//pkg': msg` — an external repository that failed to fetch
        // or configure. Counted as a load error like any other: every package
        // that depended on it is missing from the answer. Apollo's
        // `@@local_config_cuda//cuda` on a host with no CUDA is the example.
        let r = r.trim_start().strip_prefix('\'')?;
        let end = r.find('\'')?;
        return Some(r[..end].to_string());
    }
    if let Some(r) = rest.strip_prefix("package contains errors: ") {
        // `pkg: msg` — unquoted; a package name never contains a colon.
        return Some(r.split(':').next().unwrap_or(r).trim().to_string());
    }
    None
}

/// The package one line names as *declared deleted*, if it is such a line.
///
/// `--deleted_packages` is the invocation telling Bazel that a directory with a
/// `BUILD` file in it is not a package. Asking for one still exits 3 under
/// `--keep_going`, with:
///
/// ```text
/// ERROR: Skipping '//mobile/test/common/integration:*': no such package
/// 'mobile/test/common/integration': Package is considered deleted due to
/// --deleted_packages
/// ```
///
/// That is not a failure to load: the package does not exist for this
/// configuration, exactly as if its directory were gone, and a `//...` query —
/// which never names it — succeeds without it. Envoy's generated `repo.bazelrc`
/// is where the suite meets this.
pub fn parse_deleted_package_line(line: &str) -> Option<String> {
    deleted_package_of(&strip_ansi(line))
}

/// The deleted-package test, on already-stripped text.
fn deleted_package_of(l: &str) -> Option<String> {
    let l = l.trim();
    if !l.contains("considered deleted due to --deleted_packages") {
        return None;
    }
    // The last `no such package '<pkg>'` clause names the package itself; the
    // `Skipping '<pattern>'` clause before it names what was asked for.
    let r = l.rsplit_once("no such package ")?.1.trim_start();
    let r = r.strip_prefix('\'')?;
    let end = r.find('\'')?;
    Some(r[..end].to_string())
}

/// What a query's stderr said, in one pass: the packages that failed to load,
/// and the packages the invocation had declared deleted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StderrFacts {
    pub load_errors: LoadErrors,
    pub deleted_packages: Vec<String>,
}

impl StderrFacts {
    /// Folds one stderr line in.
    pub fn push_line(&mut self, line: &str) {
        let l = strip_ansi(line);
        if let Some(p) = deleted_package_of(&l) {
            if self.deleted_packages.len() < MAX_ERROR_PACKAGES {
                self.deleted_packages.push(p);
            }
            return;
        }
        if let Some(p) = parse_load_error_line(&l) {
            self.load_errors.push(&p);
        }
    }
}

/// Every fact in a stderr buffer. Used on a tail in tests; a live query folds
/// each line in as it arrives through [`StderrFacts::push_line`].
pub fn parse_stderr_facts(stderr: &str) -> StderrFacts {
    let mut f = StderrFacts::default();
    for line in stderr.lines() {
        f.push_line(line);
    }
    f
}

/// A target pattern as a query *word*, quoted when Bazel's query lexer would
/// not read it as one.
///
/// Bazel's query language is a language, not a list of labels: `+` is the
/// union operator, and its lexer ends a word at a `+` unless the word began
/// `@@` (the escape hatch added for canonical repository names like
/// `@@rules_jvm_external++maven+maven//:bar`). So `//bazel:libc++` — envoy's
/// `bool_flag` — lexes as `//bazel:libc`, `+`, `+` and the query is a syntax
/// error, *including* inside `set(...)`, which parses words like everywhere
/// else. A label with a `-` in a leading position, or one containing a space,
/// has the same problem.
///
/// Quoting is the lexer's own answer: a double-quoted word is taken literally.
/// The published run of 2026-09-08 is what this is for: every rule-proto
/// lookup for `//bazel:libc++`, `//bazel:libc++_enabled`, `//bazel:libstdc++`
/// and `//bazel:libstdc++_enabled` failed, so every classifier predicate that
/// needs the proto answered "no" and four benign extras were reported as
/// `unclassified` on five change sets.
pub fn query_word(pattern: &str) -> String {
    if pattern.contains('"') {
        // A label cannot contain a quote; anything that does is not a label
        // and is left for Bazel to reject on its own terms.
        return pattern.to_string();
    }
    if pattern.starts_with("@@") || !pattern.contains(['+', ' ']) {
        return pattern.to_string();
    }
    format!("\"{pattern}\"")
}

/// Every load error in a stderr buffer. Used on a tail in tests; a live query
/// counts them line by line through [`StderrFacts::push_line`].
pub fn parse_load_errors(stderr: &str) -> LoadErrors {
    let mut e = LoadErrors::default();
    for line in stderr.lines() {
        if let Some(p) = parse_load_error_line(line) {
            e.push(&p);
        }
    }
    e
}

/// Drops ANSI SGR sequences (`ESC [ ... m`), which Bazel emits unconditionally
/// and, on an error line, in the middle of the text as well as at the front.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // Skip up to and including the sequence's final byte.
        for c in chars.by_ref() {
            if c.is_ascii_alphabetic() {
                break;
            }
        }
    }
    out
}

/// How one Bazel process ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exit {
    /// The exit code, or `None` when a signal ended it.
    pub code: Option<i32>,
    pub success: bool,
}

impl Exit {
    fn of(status: ExitStatus) -> Self {
        Self {
            code: status.code(),
            success: status.success(),
        }
    }
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(c) => write!(f, "exit status: {c}"),
            None => f.write_str("terminated by a signal"),
        }
    }
}

/// A process run to completion with its output captured.
#[derive(Clone, Debug)]
pub struct Captured {
    pub exit: Exit,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// The process boundary: how a `Command` this crate built is actually run.
///
/// Everything above this trait — the frame reader, the stderr scrape, the
/// exit-3 rules, the partial verdicts the engine draws — is logic about what
/// Bazel *said*, and none of it needs a Bazel to be tested. [`ProcessRunner`]
/// spawns the binary; [`CannedRunner`] answers with bytes and an exit code
/// it was built with, so a unit test can hand `Bazel` a canned answer and
/// watch what the layers above make of it.
pub trait BazelRunner: std::fmt::Debug + Send + Sync {
    /// Runs `cmd` with both pipes attached: `on_stdout` is handed the live
    /// stdout stream while the process runs, every stderr line reaches
    /// `on_stderr` as it arrives, and the exit comes back once the process
    /// is done. A spawn failure is [`BazelError::Spawn`].
    fn stream(
        &self,
        cmd: &mut Command,
        on_stdout: &mut dyn FnMut(&mut dyn Read),
        on_stderr: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Captured>;

    /// Runs `cmd` to completion with stdout and stderr captured.
    fn output(&self, cmd: &mut Command) -> std::io::Result<Captured>;
}

/// The real thing: spawns the binary the command names.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessRunner;

impl BazelRunner for ProcessRunner {
    fn stream(
        &self,
        cmd: &mut Command,
        on_stdout: &mut dyn FnMut(&mut dyn Read),
        on_stderr: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Captured> {
        let binary = cmd.get_program().to_string_lossy().into_owned();
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|source| BazelError::Spawn { binary, source })?;
        let mut stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        // stderr is drained on its own thread: a chatty Bazel can fill the
        // pipe and deadlock the reader of stdout otherwise. The scope joins
        // it before the wait, so every line has been handed over by then.
        std::thread::scope(|s| {
            s.spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
                    on_stderr(&line);
                }
            });
            on_stdout(&mut stdout);
        });
        let status = child.wait()?;
        Ok(Captured {
            exit: Exit::of(status),
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }

    fn output(&self, cmd: &mut Command) -> std::io::Result<Captured> {
        let out = cmd.stdin(Stdio::null()).output()?;
        Ok(Captured {
            exit: Exit::of(out.status),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}

/// A runner that never spawns anything: the exit code, stdout bytes and
/// stderr text it was built with are what every command "produces". For the
/// unit tests of everything above the process boundary.
#[derive(Clone, Debug, Default)]
pub struct CannedRunner {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl CannedRunner {
    fn exit(&self) -> Exit {
        Exit {
            code: Some(self.code),
            success: self.code == 0,
        }
    }
}

impl BazelRunner for CannedRunner {
    fn stream(
        &self,
        _cmd: &mut Command,
        on_stdout: &mut dyn FnMut(&mut dyn Read),
        on_stderr: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Captured> {
        for line in self.stderr.lines() {
            on_stderr(line);
        }
        on_stdout(&mut &self.stdout[..]);
        Ok(Captured {
            exit: self.exit(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }

    fn output(&self, _cmd: &mut Command) -> std::io::Result<Captured> {
        Ok(Captured {
            exit: self.exit(),
            stdout: self.stdout.clone(),
            stderr: self.stderr.clone().into_bytes(),
        })
    }
}

/// The last [`STDERR_TAIL_LINES`] lines of a stderr stream, and the facts read
/// off every line of it — counted over the whole stream rather than the tail,
/// because a repository whose external deps do not resolve produces hundreds
/// of error lines and the tail keeps forty.
#[derive(Default)]
struct StderrTail {
    tail: VecDeque<String>,
    facts: StderrFacts,
}

impl StderrTail {
    fn push(&mut self, line: &str) {
        self.facts.push_line(line);
        if self.tail.len() == STDERR_TAIL_LINES {
            self.tail.pop_front();
        }
        self.tail.push_back(line.to_string());
    }

    fn finish(self) -> (String, StderrFacts) {
        (
            self.tail.into_iter().collect::<Vec<_>>().join("\n"),
            self.facts,
        )
    }
}

#[derive(Clone, Debug)]
pub struct Bazel {
    binary: PathBuf,
    workspace: PathBuf,
    extra_query_flags: Vec<String>,
    /// Startup options (`--output_base=...`, `--bazelrc=...`), placed before
    /// the command word on every invocation.
    startup_flags: Vec<String>,
    /// Extra environment for every invocation (`USE_BAZEL_VERSION` for a
    /// bazelisk wrapper, for example).
    env: Vec<(String, String)>,
    /// Directory for `--query_file` scratch files.
    scratch: PathBuf,
    /// What runs the commands this handle builds.
    runner: Arc<dyn BazelRunner>,
}

impl Bazel {
    /// Locates the workspace root by walking up from `start`, and picks the
    /// binary: explicit override, then `$BAZEL_REAL` (set when running under a
    /// bazelisk wrapper), then `$BAZEL`, then `bazelisk`, then `bazel`.
    pub fn discover(start: &Path, binary: Option<&str>) -> Result<Self> {
        let workspace = find_workspace_root(start)
            .ok_or_else(|| BazelError::NoWorkspace(start.to_path_buf()))?;
        Ok(Self::at(workspace, binary))
    }

    pub fn at(workspace: PathBuf, binary: Option<&str>) -> Self {
        let binary = binary
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("BAZEL_REAL").map(PathBuf::from))
            .or_else(|| std::env::var_os("BAZEL").map(PathBuf::from))
            .unwrap_or_else(|| {
                if which("bazelisk").is_some() {
                    PathBuf::from("bazelisk")
                } else {
                    PathBuf::from("bazel")
                }
            });
        let scratch = std::env::temp_dir();
        Self {
            binary,
            workspace,
            extra_query_flags: Vec::new(),
            startup_flags: Vec::new(),
            env: Vec::new(),
            scratch,
            runner: Arc::new(ProcessRunner),
        }
    }

    /// Runs every command this handle builds through `runner` instead of
    /// spawning the binary.
    pub fn with_runner(mut self, runner: Arc<dyn BazelRunner>) -> Self {
        self.runner = runner;
        self
    }

    /// What runs the commands this handle builds. A caller with a query shape
    /// this crate does not offer builds it on [`Bazel::command`] and runs it
    /// here, so it goes through the same seam as everything else.
    pub fn runner(&self) -> &dyn BazelRunner {
        &*self.runner
    }

    pub fn with_extra_query_flags(mut self, flags: Vec<String>) -> Self {
        self.extra_query_flags = flags;
        self
    }

    pub fn with_startup_flags(mut self, flags: Vec<String>) -> Self {
        self.startup_flags = flags;
        self
    }

    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    pub fn startup_flags(&self) -> &[String] {
        &self.startup_flags
    }

    pub fn extra_query_flags(&self) -> &[String] {
        &self.extra_query_flags
    }

    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    /// A `bazel <command>` invocation in the workspace, carrying the startup
    /// flags and environment every call here uses. Callers that need a query
    /// shape this crate does not offer (`rdeps` for verification, say) build
    /// on this rather than on a bare `Command`, so they run the same server.
    pub fn command(&self, subcommand: &str) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&self.workspace);
        cmd.args(&self.startup_flags);
        cmd.arg(subcommand);
        cmd.envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        cmd
    }

    pub fn with_scratch_dir(mut self, dir: PathBuf) -> Self {
        self.scratch = dir;
        self
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// `bazel --version`, e.g. `bazel 7.4.1`. Empty string if it cannot be
    /// determined; the version is recorded in the index header, not acted on.
    pub fn version(&self) -> String {
        // Not [`command`](Self::command): Bazel refuses `--version` after a
        // startup option (`bazel --output_base=… --version` exits 2), and the
        // benchmark always passes one, which is how a whole run recorded no
        // Bazel version at all. The workspace and the environment stay, so
        // bazelisk reads `.bazelversion` and `USE_BAZEL_VERSION` as it would.
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&self.workspace)
            .arg("--version")
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        match self.runner.output(&mut cmd) {
            Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            Err(_) => String::new(),
        }
    }

    /// Runs `bazel query` for a set of packages, streaming frames to `on_batch`.
    ///
    /// `//pkg:*` loads exactly that package and does not walk its dependencies,
    /// so the cost is proportional to the dirty set (spec 7.1).
    pub fn query_packages(
        &self,
        packages: &[String],
        on_batch: impl FnMut(&[u8], &[(usize, usize)]) -> std::result::Result<(), ReadError>,
    ) -> Result<QueryOutcome> {
        if packages.is_empty() {
            return Ok(QueryOutcome {
                exit_code: Some(0),
                success: true,
                stderr_tail: String::new(),
                targets_read: 0,
                load_errors: LoadErrors::default(),
                deleted_packages: Vec::new(),
            });
        }
        let mut expr = String::from("set(");
        for (i, p) in packages.iter().enumerate() {
            if i > 0 {
                expr.push(' ');
            }
            expr.push_str("//");
            expr.push_str(p);
            expr.push_str(":*");
        }
        expr.push(')');
        self.run_query(&expr, on_batch)
    }

    /// Runs the whole-repository query for a cold build (spec 6.9).
    ///
    /// `//...:*`, not `//...`: the latter is shorthand for `//...:all`, which
    /// matches rules only. A cold index built from it would carry file labels
    /// as unknown nodes while an incremental `//pkg:*` query records them as
    /// source files — the same graph described two different ways, which makes
    /// a cold index and a spliced one incomparable.
    pub fn query_universe(
        &self,
        on_batch: impl FnMut(&[u8], &[(usize, usize)]) -> std::result::Result<(), ReadError>,
    ) -> Result<QueryOutcome> {
        self.run_query("//...:*", on_batch)
    }

    fn run_query(
        &self,
        expr: &str,
        on_batch: impl FnMut(&[u8], &[(usize, usize)]) -> std::result::Result<(), ReadError>,
    ) -> Result<QueryOutcome> {
        let mut cmd = self.command("query");

        // Long expressions go in a file: argv limits are OS-dependent and a
        // dirty set of thousands of packages will exceed them.
        let _scratch_file = if expr.len() > MAX_INLINE_QUERY {
            let path = self.scratch.join(format!(
                "cambium-query-{}-{}.txt",
                std::process::id(),
                now_nanos()
            ));
            std::fs::write(&path, expr)?;
            cmd.arg(format!("--query_file={}", path.display()));
            Some(ScratchFile(path))
        } else {
            cmd.arg(expr);
            None
        };

        for f in QUERY_FLAGS.iter().chain(HYGIENE.iter()) {
            cmd.arg(f);
        }
        for f in &self.extra_query_flags {
            cmd.arg(f);
        }

        tracing::debug!(
            binary = %self.binary.display(),
            expr_len = expr.len(),
            query = %truncate(expr, 200),
            "running bazel query"
        );

        let mut on_batch = on_batch;
        let mut targets_read: std::result::Result<usize, ReadError> = Ok(0);
        let mut stderr = StderrTail::default();
        let done = self.runner.stream(
            &mut cmd,
            &mut |stdout| {
                targets_read = read_frames(
                    BufReader::with_capacity(1 << 20, stdout),
                    cambium_proto::reader::DEFAULT_BATCH,
                    &mut on_batch,
                );
            },
            &mut |line| stderr.push(line),
        )?;
        let (stderr_tail, facts) = stderr.finish();

        // A read error usually means bazel died mid-stream; the stderr tail is
        // the useful part of that story, so report it rather than the truncation.
        let targets_read = match targets_read {
            Ok(n) => n,
            Err(e) => {
                if done.exit.success {
                    return Err(e.into());
                }
                tracing::debug!(error = %e, "bazel stream ended early");
                0
            }
        };

        Ok(QueryOutcome {
            exit_code: done.exit.code,
            success: done.exit.success,
            stderr_tail,
            targets_read,
            load_errors: facts.load_errors,
            deleted_packages: facts.deleted_packages,
        })
    }

    /// `rbuildfiles(...)`: what loads any of `bzl_paths` (spec 7.2). Paths are
    /// workspace-relative; see [`Rbuildfiles`] for why the answer is not just
    /// a list of packages. The `bool` says Sky Query finished with errors, so
    /// the answer is whatever it managed.
    ///
    /// **Oracle only.** The engine reads this out of the index's static load
    /// closure instead; nothing on the `affected` path calls Bazel here. A
    /// partial `rbuildfiles` can be missing exactly the package that loads the
    /// changed file, and an under-approximation is unusable for an answer. The
    /// comparison suite can live with one, because it only asks whether
    /// cambium's answer *contains* Bazel's.
    pub fn rbuildfiles(&self, bzl_paths: &[String]) -> Result<(Rbuildfiles, bool)> {
        if bzl_paths.is_empty() {
            return Ok((Rbuildfiles::default(), false));
        }
        let expr = format!("rbuildfiles({})", bzl_paths.join(", "));
        let mut cmd = self.command("query");
        cmd.arg(&expr)
            // rbuildfiles needs Sky Query mode, which --universe_scope enables.
            .arg("--universe_scope=//...")
            .arg("--order_output=no")
            .arg("--output=label")
            .arg("--keep_going");
        for f in HYGIENE {
            cmd.arg(f);
        }

        let mut out = String::new();
        let mut read: std::io::Result<usize> = Ok(0);
        let mut stderr = StderrTail::default();
        let done = self.runner.stream(
            &mut cmd,
            &mut |stdout| read = stdout.read_to_string(&mut out),
            &mut |line| stderr.push(line),
        )?;
        read?;
        let (stderr_tail, _) = stderr.finish();
        // Exit 3 is "finished with errors" under --keep_going: the answer is
        // whatever loaded. Whether that is usable is the caller's call.
        if done.exit.code == Some(3) {
            return Ok((parse_buildfile_labels(&out), true));
        }
        if !done.exit.success {
            return Err(BazelError::Failed {
                args: truncate(&expr, 200),
                status: done.exit.to_string(),
                stderr: stderr_tail,
            });
        }
        Ok((parse_buildfile_labels(&out), false))
    }

    /// The root module's repository mapping as Bazel prints it, for
    /// `cambium_index::RepoMapping::from_dump` to parse.
    ///
    /// `bazel mod dump_repo_mapping ""` (Bazel >= 7.1) prints it as one JSON
    /// object on stdout: apparent repository name -> canonical name. Measured
    /// output shapes per Bazel version are recorded in `cambium_index::loads`. Anything that goes wrong — the subcommand does
    /// not exist, the repository is in WORKSPACE mode, the module graph will
    /// not resolve — is a warning and the identity mapping, which is what
    /// WORKSPACE mode means anyway. The mapping only ever *names* external
    /// loads; nothing walks into them, so a wrong name costs nothing but a
    /// less precise record.
    pub fn repo_mapping_dump(&self) -> Option<String> {
        let mut cmd = self.command("mod");
        cmd.arg("dump_repo_mapping").arg("");
        match self.runner.output(&mut cmd) {
            Ok(o) if o.exit.success => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
            Ok(o) => {
                // Once per process: a replay asks for the mapping on every
                // step, and a WORKSPACE repository answers the same way each
                // time.
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    tracing::warn!(
                        status = %o.exit,
                        "bazel mod dump_repo_mapping failed (WORKSPACE mode, or bazel < 7.1); \
                         falling back to the identity repository mapping"
                    )
                });
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not run bazel mod dump_repo_mapping");
                None
            }
        }
    }
}

/// What `rbuildfiles` said, split by what cambium can act on.
///
/// `rbuildfiles` answers with labels, and only some of them name a main-repo
/// package. A `.bzl` defining repository rules comes back naming the
/// *external* repository it builds, or `//external:WORKSPACE`, and never a
/// package — so flattening the answer to "no packages" would mark nothing
/// dirty while the contents of every external repository changed underneath.
///
/// Observed on 9.2.0 / 8.5.1 / 7.4.1 against a workspace whose `//app` target
/// depends on `@my_repo//:x`:
///
/// | mode | query | answer |
/// |---|---|---|
/// | bzlmod (all three) | `rbuildfiles(bazel/ext.bzl)` | `@my_repo//:BUILD` |
/// | bzlmod (all three) | `rbuildfiles(MODULE.bazel)` | *(empty)* |
/// | bzlmod (all three) | `rbuildfiles(WORKSPACE)` | every package, `@my_repo//:BUILD`, `@bazel_tools//tools/genrule:BUILD` |
/// | WORKSPACE (7.4.1, 8.5.1 with `--noenable_bzlmod --enable_workspace`) | `rbuildfiles(bazel/ext.bzl)` | `//external:WORKSPACE`, `@my_repo//:BUILD`, every package |
/// | either, any version | `rbuildfiles(<.bzl nothing loads>)` | *(empty)* |
///
/// So a repository-rule `.bzl` is never silent, and a genuinely dead one is
/// the only thing that comes back empty — which is why anything non-empty
/// that is not a main-repo package is treated as global.
///
/// Oracle only: the engine reads the same answer out of the index's static
/// load closure (`cambium_index::loads`). The table is kept because it is what
/// the oracle (`cambium_compare::bzloracle`) is calibrated against.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rbuildfiles {
    /// Main-repository packages whose `BUILD` loads one of the paths — the
    /// set the engine now reads out of the load closure instead, and that the
    /// comparison suite checks against this.
    pub packages: Vec<String>,
    /// Labels that name a loader outside any main-repo package:
    /// `//:MODULE.bazel`, `//:WORKSPACE[.bazel|.bzlmod]`, `//external:*`, and
    /// any `@repo//...` label. Non-empty means the change reaches external
    /// repository contents, which cambium's index does not model — global.
    pub workspace_level: Vec<String>,
}

impl Rbuildfiles {
    /// Nothing at all loads the path: not a package, not the workspace files,
    /// not an external repository. A dead file.
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty() && self.workspace_level.is_empty()
    }
}

/// Parses `--output=label` lines from `rbuildfiles`.
///
/// `//lib/core:BUILD` -> a package; `@my_repo//:BUILD`, `//external:WORKSPACE`,
/// `//:MODULE.bazel` -> workspace-level. Other shapes are skipped.
pub fn parse_buildfile_labels(out: &str) -> Rbuildfiles {
    const WORKSPACE_FILES: &[&str] = &[
        "MODULE.bazel",
        "MODULE.bazel.lock",
        "WORKSPACE",
        "WORKSPACE.bazel",
        "WORKSPACE.bzlmod",
        "REPO.bazel",
    ];
    let mut r = Rbuildfiles::default();
    for line in out.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        // Any label in another repository: `@repo//...` or `@@canonical//...`.
        if l.starts_with('@') {
            r.workspace_level.push(l.to_string());
            continue;
        }
        let Some(rest) = l.strip_prefix("//") else {
            continue;
        };
        let Some((pkg, name)) = rest.rsplit_once(':') else {
            continue;
        };
        // `//external:...` is the pseudo-package Bazel reports WORKSPACE-driven
        // loads under in WORKSPACE mode; `//:MODULE.bazel` and friends are the
        // workspace files themselves.
        let external_pseudo_package = pkg == "external" || pkg.starts_with("external/");
        let workspace_file = pkg.is_empty() && WORKSPACE_FILES.contains(&name);
        if external_pseudo_package || workspace_file {
            r.workspace_level.push(l.to_string());
        } else if name == "BUILD" || name == "BUILD.bazel" {
            r.packages.push(pkg.to_string());
        }
    }
    r.packages.sort();
    r.packages.dedup();
    r.workspace_level.sort();
    r.workspace_level.dedup();
    r
}

/// Walks up from `start` looking for a workspace marker.
pub fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    const MARKERS: &[&str] = &[
        "MODULE.bazel",
        "WORKSPACE",
        "WORKSPACE.bazel",
        "WORKSPACE.bzlmod",
    ];
    let mut dir = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    loop {
        if MARKERS.iter().any(|m| dir.join(m).exists()) {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

struct ScratchFile(PathBuf);

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let p = dir.join(bin);
        p.is_file().then_some(p)
    })
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}... ({} bytes)", &s[..n], s.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim from a `bazel 9.2.0 query //...:*` over ApolloAuto/apollo,
    /// colour codes and all: the repository's deps live in WORKSPACE, which
    /// Bazel 9 no longer reads, so nearly every package fails to load.
    const APOLLO_STDERR: &str = concat!(
        "\u{1b}[31m\u{1b}[1mERROR: \u{1b}[0merror loading package 'modules/planning/traffic_rules/crosswalk': at /w/tools/cpplint.bzl:3:6: Unable to find package for @@[unknown repo 'rules_python' requested from @@]//python:defs.bzl\n",
        "\u{1b}[31m\u{1b}[1mERROR: \u{1b}[0merror loading package '': compilation of module 'tools/install/install.bzl' failed\n",
        "\u{1b}[31m\u{1b}[1mERROR: \u{1b}[0mpackage contains errors: third_party/bazel: name 'py_library' is not defined (did you mean 'cc_library'?)\n",
        "\u{1b}[35mWARNING: \u{1b}[0m--keep_going specified, ignoring errors. Results may be inaccurate\n",
    );

    fn rule(name: &str) -> cambium_proto::blaze_query::Target {
        use cambium_proto::blaze_query::{target::Discriminator, Rule, Target};
        Target {
            r#type: Discriminator::Rule as i32,
            rule: Some(Rule {
                name: name.to_string(),
                rule_class: "cc_library".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn canned(code: i32, stdout: Vec<u8>, stderr: &str) -> Bazel {
        Bazel::at(std::env::temp_dir(), Some("bazel")).with_runner(Arc::new(CannedRunner {
            code,
            stdout,
            stderr: stderr.to_string(),
        }))
    }

    /// Everything a query reads off a process — the frames, the exit, the
    /// tail, the facts scraped from every stderr line — through a runner
    /// that never spawned one.
    #[test]
    fn a_query_through_a_canned_runner_reads_frames_and_scrapes_stderr() {
        let bytes = cambium_proto::encode_stream(&[rule("//a:x"), rule("//a:y")]);
        let bazel = canned(3, bytes, APOLLO_STDERR);
        let mut frames = 0;
        let out = bazel
            .query_packages(&["a".into()], |_, ranges| {
                frames += ranges.len();
                Ok(())
            })
            .unwrap();
        assert_eq!(frames, 2);
        assert_eq!(out.targets_read, 2);
        assert_eq!(out.exit_code, Some(3));
        assert!(!out.success);
        assert_eq!(out.load_errors.count, 3);
        assert!(
            out.stderr_tail.contains("py_library"),
            "{}",
            out.stderr_tail
        );
    }

    #[test]
    fn a_universe_that_exits_zero_is_a_success_with_no_facts() {
        let bazel = canned(0, cambium_proto::encode_stream(&[rule("//a:x")]), "");
        let out = bazel.query_universe(|_, _| Ok(())).unwrap();
        assert!(out.success);
        assert_eq!(out.exit_code, Some(0));
        assert!(out.load_errors.is_empty());
        assert!(out.deleted_packages.is_empty());
    }

    /// A stream that ends mid-frame under a successful exit is a read error;
    /// under a failed exit it is reported as the failure it came with.
    #[test]
    fn a_truncated_stream_is_an_error_only_when_bazel_claimed_success() {
        let mut bytes = cambium_proto::encode_stream(&[rule("//a:x")]);
        bytes.truncate(bytes.len() - 2);
        assert!(matches!(
            canned(0, bytes.clone(), "").query_universe(|_, _| Ok(())),
            Err(BazelError::Read(_))
        ));
        let out = canned(1, bytes, "ERROR: boom")
            .query_universe(|_, _| Ok(()))
            .unwrap();
        assert_eq!(out.exit_code, Some(1));
        assert_eq!(out.targets_read, 0);
        assert_eq!(out.stderr_tail, "ERROR: boom");
    }

    #[test]
    fn rbuildfiles_through_a_canned_runner_splits_loaders_and_reports_exit_3() {
        let bazel = canned(3, b"//lib:BUILD\n@x//:BUILD\n".to_vec(), "");
        let (r, partial) = bazel.rbuildfiles(&["a.bzl".into()]).unwrap();
        assert!(partial);
        assert_eq!(r.packages, vec!["lib".to_string()]);
        assert_eq!(r.workspace_level, vec!["@x//:BUILD".to_string()]);
        let err = canned(1, Vec::new(), "ERROR: nope")
            .rbuildfiles(&["a.bzl".into()])
            .unwrap_err();
        assert!(matches!(err, BazelError::Failed { ref stderr, .. } if stderr == "ERROR: nope"));
    }

    #[test]
    fn the_version_and_the_mapping_dump_come_off_the_runner_too() {
        let bazel = canned(0, b"bazel 7.4.1\n".to_vec(), "");
        assert_eq!(bazel.version(), "bazel 7.4.1");
        assert_eq!(bazel.repo_mapping_dump().as_deref(), Some("bazel 7.4.1\n"));
        assert_eq!(canned(2, Vec::new(), "").repo_mapping_dump(), None);
    }

    #[test]
    fn a_binary_that_cannot_be_spawned_is_named() {
        let bazel = Bazel::at(std::env::temp_dir(), Some("/nonexistent/cambium-bazel"));
        let err = bazel.query_universe(|_, _| Ok(())).unwrap_err();
        assert!(
            matches!(err, BazelError::Spawn { ref binary, .. } if binary.contains("cambium-bazel")),
            "{err}"
        );
    }

    #[test]
    fn a_label_the_query_lexer_would_split_is_quoted() {
        // envoy's `bool_flag`s and the `config_setting`s over them: `+` is the
        // union operator, so unquoted these are three tokens, not one.
        assert_eq!(query_word("//bazel:libc++"), "\"//bazel:libc++\"");
        assert_eq!(
            query_word("//bazel:libstdc++_enabled"),
            "\"//bazel:libstdc++_enabled\""
        );
        // Ordinary labels are left alone, so the queries the suite already
        // sends do not change shape.
        assert_eq!(query_word("//bazel:envoy"), "//bazel:envoy");
        assert_eq!(query_word("//:BUILD"), "//:BUILD");
        // A canonical repository name already lexes as one word, and quoting
        // it would be noise in every log line.
        assert_eq!(
            query_word("@@rules_jvm_external++maven+maven//:bar"),
            "@@rules_jvm_external++maven+maven//:bar"
        );
    }

    #[test]
    fn parses_load_errors_from_coloured_stderr() {
        let e = parse_load_errors(APOLLO_STDERR);
        assert_eq!(e.count, 3);
        assert_eq!(
            e.packages,
            vec![
                "modules/planning/traffic_rules/crosswalk".to_string(),
                // The root package is a real package name, not "no package".
                String::new(),
                "third_party/bazel".to_string(),
            ]
        );
    }

    #[test]
    fn ignores_lines_that_are_not_load_errors() {
        for line in [
            "INFO: Elapsed time: 3.2s",
            "WARNING: --keep_going specified, ignoring errors.",
            "ERROR: Skipping '//foo:bar': no such target",
            "ERROR: Analysis of target '//x:y' failed",
            "",
        ] {
            assert_eq!(parse_load_error_line(line), None, "{line:?}");
        }
    }

    #[test]
    fn counts_an_external_repository_that_would_not_load() {
        assert_eq!(
            parse_load_error_line(
                "\u{1b}[31mERROR: \u{1b}[0mno such package '@@local_config_cuda//cuda': \
                 Cuda Configuration Error"
            )
            .as_deref(),
            Some("@@local_config_cuda//cuda")
        );
    }

    /// The line envoy's own `--deleted_packages` produces when a dirty package
    /// under `mobile/test/common/integration` is asked for by name.
    const DELETED_LINE: &str = "ERROR: Skipping '//mobile/test/common/integration:*': \
         no such package 'mobile/test/common/integration': Package is considered deleted \
         due to --deleted_packages";

    #[test]
    fn a_declared_deleted_package_is_not_a_load_error() {
        // It carries a `no such package '...'` clause, so before this it was
        // counted as one — and a run that touched such a package published an
        // index flagged partial with no error lines to show for it.
        assert_eq!(parse_load_error_line(DELETED_LINE), None);
        assert_eq!(
            parse_deleted_package_line(DELETED_LINE).as_deref(),
            Some("mobile/test/common/integration")
        );
        let f = parse_stderr_facts(&format!("{DELETED_LINE}\nINFO: Elapsed time: 3.2s\n"));
        assert!(f.load_errors.is_empty());
        assert_eq!(f.deleted_packages, ["mobile/test/common/integration"]);
    }

    #[test]
    fn a_load_failure_next_to_a_deleted_package_still_counts() {
        let f = parse_stderr_facts(&format!(
            "{DELETED_LINE}\nERROR: error loading package 'modules/planning': boom\n"
        ));
        assert_eq!(f.load_errors.count, 1);
        assert_eq!(f.load_errors.packages, ["modules/planning".to_string()]);
        assert_eq!(f.deleted_packages.len(), 1);
    }

    #[test]
    fn keeps_only_the_first_n_package_names_but_counts_them_all() {
        let mut s = String::new();
        for i in 0..(MAX_ERROR_PACKAGES + 5) {
            s.push_str(&format!("ERROR: error loading package 'p{i}': boom\n"));
        }
        let e = parse_load_errors(&s);
        assert_eq!(e.count as usize, MAX_ERROR_PACKAGES + 5);
        assert_eq!(e.packages.len(), MAX_ERROR_PACKAGES);
        assert_eq!(e.packages[0], "p0");
    }

    #[test]
    fn finds_the_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("MODULE.bazel"), "").unwrap();
        let deep = root.join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(
            find_workspace_root(&deep).unwrap().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn reports_a_missing_workspace() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_workspace_root(dir.path()).is_none());
    }

    #[test]
    fn parses_rbuildfiles_output() {
        let out = "//lib/core:BUILD\n//app:BUILD.bazel\n//:BUILD\n//weird:thing\ngarbage\n";
        let r = parse_buildfile_labels(out);
        assert_eq!(r.packages, vec!["", "app", "lib/core"]);
        assert!(r.workspace_level.is_empty());
        assert!(!r.is_empty());
    }

    #[test]
    fn separates_workspace_level_loaders_from_packages() {
        // bzlmod, 9.2.0 / 8.5.1 / 7.4.1: a module-extension `.bzl` names only
        // the external repository it defines.
        let r = parse_buildfile_labels("@my_repo//:BUILD\n");
        assert!(r.packages.is_empty());
        assert_eq!(r.workspace_level, vec!["@my_repo//:BUILD"]);
        assert!(!r.is_empty());

        // WORKSPACE mode, 7.4.1 / 8.5.1: the pseudo-package plus everything.
        let r = parse_buildfile_labels(
            "//external:WORKSPACE\n@my_repo//:BUILD\n//app:BUILD\n//bazel:BUILD\n\
             @bazel_tools//tools/genrule:BUILD\n",
        );
        assert_eq!(r.packages, vec!["app", "bazel"]);
        assert_eq!(
            r.workspace_level,
            vec![
                "//external:WORKSPACE",
                "@bazel_tools//tools/genrule:BUILD",
                "@my_repo//:BUILD"
            ]
        );

        // The workspace files themselves, and canonical `@@` repo names.
        let r =
            parse_buildfile_labels("//:MODULE.bazel\n//:WORKSPACE.bazel\n@@rules_cc+//:BUILD\n");
        assert!(r.packages.is_empty());
        assert_eq!(r.workspace_level.len(), 3);

        // A dead `.bzl`: nothing at all.
        assert!(parse_buildfile_labels("").is_empty());
        assert!(parse_buildfile_labels("INFO: Empty results\n").is_empty());
    }
}
