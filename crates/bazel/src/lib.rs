//! Spawning Bazel.
//!
//! Everything here is deliberately thin: two query shapes (a package set and
//! the whole repo), one `rbuildfiles` call, and version detection. The flags
//! are pinned to what the spec's core invariant needs — unconfigured graph,
//! unflattened selects, implicit deps left in — and are the only Bazel surface
//! cambium depends on.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;

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

/// Every load error in a stderr buffer. Used on a tail in tests, and on the
/// live stream by [`drain_stderr`].
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
        }
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
        let out = self
            .command("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        match out {
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

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|source| BazelError::Spawn {
            binary: self.binary.display().to_string(),
            source,
        })?;

        let stderr_rx = drain_stderr(&mut child);
        let stdout = child.stdout.take().expect("stdout piped");
        let targets_read = read_frames(
            BufReader::with_capacity(1 << 20, stdout),
            cambium_proto::reader::DEFAULT_BATCH,
            on_batch,
        );

        let status = child.wait()?;
        let (stderr_tail, load_errors) = stderr_rx.recv().unwrap_or_default();

        // A read error usually means bazel died mid-stream; the stderr tail is
        // the useful part of that story, so report it rather than the truncation.
        let targets_read = match targets_read {
            Ok(n) => n,
            Err(e) => {
                if status.success() {
                    return Err(e.into());
                }
                tracing::debug!(error = %e, "bazel stream ended early");
                0
            }
        };

        Ok(QueryOutcome {
            exit_code: status.code(),
            success: status.success(),
            stderr_tail,
            targets_read,
            load_errors,
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
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|source| BazelError::Spawn {
            binary: self.binary.display().to_string(),
            source,
        })?;
        let stderr_rx = drain_stderr(&mut child);
        let mut out = String::new();
        child
            .stdout
            .take()
            .expect("stdout piped")
            .read_to_string(&mut out)?;
        let status = child.wait()?;
        let (stderr_tail, _) = stderr_rx.recv().unwrap_or_default();
        // Exit 3 is "finished with errors" under --keep_going: the answer is
        // whatever loaded. Whether that is usable is the caller's call.
        if status.code() == Some(3) {
            return Ok((parse_buildfile_labels(&out), true));
        }
        if !status.success() {
            return Err(BazelError::Failed {
                args: truncate(&expr, 200),
                status: status.to_string(),
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
        let out = self
            .command("mod")
            .arg("dump_repo_mapping")
            .arg("")
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .output();
        match out {
            Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
            Ok(o) => {
                // Once per process: a replay asks for the mapping on every
                // step, and a WORKSPACE repository answers the same way each
                // time.
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    tracing::warn!(
                        status = %o.status,
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

/// Drains the child's stderr on a thread (otherwise a chatty Bazel can fill the
/// pipe and deadlock us), keeping only the tail.
fn drain_stderr(child: &mut Child) -> mpsc::Receiver<(String, LoadErrors)> {
    let (tx, rx) = mpsc::channel();
    let Some(stderr) = child.stderr.take() else {
        let _ = tx.send((String::new(), LoadErrors::default()));
        return rx;
    };
    std::thread::spawn(move || {
        let mut tail: std::collections::VecDeque<String> =
            std::collections::VecDeque::with_capacity(STDERR_TAIL_LINES);
        // Counted here rather than from `tail`: a repository whose external
        // deps do not resolve produces hundreds of these, and the tail keeps
        // forty lines.
        let mut errors = LoadErrors::default();
        for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
            if let Some(p) = parse_load_error_line(&line) {
                errors.push(&p);
            }
            if tail.len() == STDERR_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
        let _ = tx.send((tail.into_iter().collect::<Vec<_>>().join("\n"), errors));
    });
    rx
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
