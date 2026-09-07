//! The bazel-diff [`Producer`].
//!
//! bazel-diff answers the same question cambium does by a different route: it
//! runs one whole-repository `bazel query` per revision, hashes every target's
//! definition transitively into a `{label: hash}` JSON file, and diffs the two
//! files. That shape is why the comparison is worth making — it is the tool CI
//! systems actually use — and it is also why the timings are reported in two
//! flavours:
//!
//! - **cold**: hash base, hash head, diff. What a cache-less run costs.
//! - **warm**: hash head, diff, reusing yesterday's base hash file. What a CI
//!   system that persists hashes actually pays per commit.
//!
//! Both implementations (the Rust binary and the Kotlin jar) take the same
//! flags, so they differ here only in how the process is spawned and in the
//! version string.
//!
//! # Flags
//!
//! `generate-hashes` is run with **`-k` (`--keep_going`)** by default. On a
//! real repository at a real pin, `bazel query '//...:all-targets'` does not
//! load cleanly: `bazel/bazel` on macOS references a GraalVM distribution with
//! no `macos-x64` build and an RBE config that shells out to Docker, so the
//! query exits 3 and bazel-diff, without `-k`, fails the pair outright. With
//! `-k` it hashes everything that loaded — the same universe cambium indexes
//! with its own `--keep_going`, and the same universe the `rdeps` oracle sees
//! (see [`crate::oracle`]). Turn it off with
//! [`BazelDiff::with_keep_going`] if a repository loads cleanly and a partial
//! answer should be an error instead. No `--useCquery` and no
//! `--fineGrainedHashExternalRepos`: both change what "affected" means.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context as _, Result};

use crate::producer::{normalise_label, run_capture, Producer, ProducerRun, RunContext};

/// Environment overrides, in the order they are consulted.
pub const BIN_ENV: &str = "CAMBIUM_BAZEL_DIFF";
pub const DIR_ENV: &str = "CAMBIUM_BAZEL_DIFF_DIR";
pub const JAR_ENV: &str = "CAMBIUM_BAZEL_DIFF_JAR";

/// Which bazel-diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Implementation {
    /// The Rust rewrite, built by `tools/bazel-diff/fetch.sh`.
    Rust,
    /// The original, `java -jar bazel-diff_deploy.jar`.
    Kotlin,
}

impl Implementation {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "rust" => Some(Self::Rust),
            "kotlin" | "jvm" | "java" => Some(Self::Kotlin),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "bazel-diff-rust",
            Self::Kotlin => "bazel-diff-kotlin",
        }
    }
}

#[derive(Clone, Debug)]
enum Exec {
    Binary(PathBuf),
    Jar { java: PathBuf, jar: PathBuf },
}

/// A located bazel-diff installation.
#[derive(Clone, Debug)]
pub struct BazelDiff {
    imp: Implementation,
    exec: Exec,
    version: String,
    name: String,
    /// `-k`, on by default; see the module docs.
    keep_going: bool,
}

/// `~/.cache/cambium/bazel-diff`, where `tools/bazel-diff/fetch.sh` installs.
pub fn default_dir() -> PathBuf {
    if let Some(d) = std::env::var_os(DIR_ENV) {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(".cache/cambium/bazel-diff")
}

/// The version fetch.sh recorded next to `bin/`, if it did.
///
/// Preferred over asking the binary: the Rust crate's own version lags the
/// repository tag (the tag-v46 build reports `bazel-diff 39.0.0`), and it is
/// the tag and commit that identify what was compared.
pub fn read_version_file(dir: &Path) -> Option<String> {
    let s = std::fs::read_to_string(dir.join("VERSION")).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.lines().collect::<Vec<_>>().join(" "))
}

impl BazelDiff {
    /// Locates the requested implementation, or explains where it looked.
    pub fn discover(imp: Implementation) -> Result<Self> {
        let dir = default_dir();
        match imp {
            Implementation::Rust => {
                let bin = std::env::var_os(BIN_ENV)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| dir.join("bin/bazel-diff-rust"));
                anyhow::ensure!(
                    bin.exists(),
                    "no bazel-diff binary at {}; set ${BIN_ENV}, or ${DIR_ENV}, \
                     or run tools/bazel-diff/fetch.sh",
                    bin.display()
                );
                let version = read_version_file(&dir)
                    .or_else(|| binary_version(&bin))
                    .unwrap_or_else(|| "unknown".into());
                Ok(Self::new(imp, Exec::Binary(bin), version))
            }
            Implementation::Kotlin => {
                let jar = std::env::var_os(JAR_ENV)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| dir.join("bazel-diff_deploy.jar"));
                anyhow::ensure!(
                    jar.exists(),
                    "no bazel-diff jar at {}; set ${JAR_ENV}, or ${DIR_ENV}",
                    jar.display()
                );
                let java = std::env::var_os("JAVA_HOME")
                    .map(|h| PathBuf::from(h).join("bin/java"))
                    .filter(|p| p.exists())
                    .unwrap_or_else(|| PathBuf::from("java"));
                let version = read_version_file(&dir).unwrap_or_else(|| "unknown".into());
                Ok(Self::new(imp, Exec::Jar { java, jar }, version))
            }
        }
    }

    /// An installation at a known path, for callers with their own layout.
    pub fn at_binary(path: PathBuf, version: String) -> Self {
        Self::new(Implementation::Rust, Exec::Binary(path), version)
    }

    fn new(imp: Implementation, exec: Exec, version: String) -> Self {
        Self {
            imp,
            exec,
            version,
            name: imp.name().to_string(),
            keep_going: true,
        }
    }

    /// Whether to pass `-k` to `generate-hashes`. On by default.
    pub fn with_keep_going(mut self, yes: bool) -> Self {
        self.keep_going = yes;
        self
    }

    pub fn keep_going(&self) -> bool {
        self.keep_going
    }

    pub fn implementation(&self) -> Implementation {
        self.imp
    }

    fn command(&self, ctx: &RunContext) -> Command {
        match &self.exec {
            Exec::Binary(p) => ctx.command(p),
            Exec::Jar { java, jar } => {
                let mut c = ctx.command(java);
                c.arg("-jar").arg(jar);
                c
            }
        }
    }

    /// `generate-hashes` for one workspace, into `out`.
    ///
    /// The startup options are the corpus ones minus `--output_base`, so
    /// bazel-diff gets one output base per workspace directory rather than
    /// sharing cambium's — see [`RunContext::producer_startup_flags`] for what
    /// sharing it costs. `-k` rides along unless it was turned off; see the
    /// module docs for why it is the default.
    pub fn generate_hashes(
        &self,
        ws: &Path,
        out: &Path,
        ctx: &RunContext,
        command_options: &[String],
    ) -> Result<Duration> {
        let mut cmd = self.command(ctx);
        cmd.arg("generate-hashes")
            .arg("-w")
            .arg(ws)
            .arg("-b")
            .arg(&ctx.bazel_binary);
        if self.keep_going {
            cmd.arg("-k");
        }
        let startup = ctx.producer_startup_flags();
        if !startup.is_empty() {
            cmd.arg("--bazelStartupOptions").arg(startup.join(" "));
        }
        if !command_options.is_empty() {
            cmd.arg("--bazelCommandOptions")
                .arg(command_options.join(" "));
        }
        cmd.arg(out);
        let c = run_capture(
            &mut cmd,
            &format!("{} generate-hashes at {}", self.name, ws.display()),
        )?;
        Ok(c.wall)
    }
}

/// The labels a tool reported, and the distances it attached to them.
pub type Impacted = (BTreeSet<String>, Option<BTreeMap<String, u32>>);

impl BazelDiff {
    /// `get-impacted-targets` over two hash files.
    pub fn get_impacted_targets(
        &self,
        ws: &Path,
        starting: &Path,
        final_: &Path,
        out: &Path,
        ctx: &RunContext,
    ) -> Result<(Impacted, Duration)> {
        let mut cmd = self.command(ctx);
        cmd.arg("get-impacted-targets")
            .arg("--startingHashes")
            .arg(starting)
            .arg("--finalHashes")
            .arg(final_)
            .arg("-o")
            .arg(out)
            .arg("-w")
            .arg(ws)
            .arg("-b")
            .arg(&ctx.bazel_binary);
        let c = run_capture(&mut cmd, &format!("{} get-impacted-targets", self.name))?;
        // Older builds print to stdout when `-o` is not honoured; prefer the
        // file and fall back, so a version skew does not read as "nothing
        // changed".
        let text = std::fs::read_to_string(out).unwrap_or(c.stdout);
        Ok((parse_impacted(&text), c.wall))
    }
}

impl Producer for BazelDiff {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> String {
        self.version.clone()
    }

    fn affected(&self, ws: &Path, base: &str, head: &str, ctx: &RunContext) -> Result<ProducerRun> {
        std::fs::create_dir_all(&ctx.scratch)?;
        let short = |s: &str| s.chars().take(12).collect::<String>();
        let base_hashes = ctx.scratch_file(&format!("{}-{}.json", self.name, short(base)));
        let head_hashes = ctx.scratch_file(&format!("{}-{}.json", self.name, short(head)));
        let out = ctx.scratch_file(&format!(
            "{}-{}-{}.txt",
            self.name,
            short(base),
            short(head)
        ));

        // The base hash runs against the second worktree; the head hash
        // against cambium's own checkout, which is already at head.
        let base_wall = self
            .generate_hashes(&ctx.base_worktree, &base_hashes, ctx, &[])
            .with_context(|| format!("hashing {base} in {}", ctx.base_worktree.display()))?;
        let head_wall = self
            .generate_hashes(ws, &head_hashes, ctx, &[])
            .with_context(|| format!("hashing {head} in {}", ws.display()))?;
        let ((labels, depths), diff_wall) =
            self.get_impacted_targets(ws, &base_hashes, &head_hashes, &out, ctx)?;

        Ok(ProducerRun {
            labels,
            cold_wall: base_wall + head_wall + diff_wall,
            warm_wall: Some(head_wall + diff_wall),
            raw_output_path: Some(out),
            depths,
            dropped_file_targets: 0,
        })
    }
}

fn binary_version(bin: &Path) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Parses `get-impacted-targets` output: one label per line, or, when
/// `--depEdgesFile` was used, a JSON array of
/// `{label, targetDistance, packageDistance}`.
///
/// Labels go through [`normalise_label`] so an external target has the same
/// spelling here as it does in Tango's and cambium's output; without that a
/// canonical-vs-apparent repository name puts one target on *both* sides of
/// the symmetric difference.
pub fn parse_impacted(text: &str) -> Impacted {
    let trimmed = text.trim_start();
    if trimmed.starts_with('[') {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Entry {
            label: String,
            target_distance: Option<i64>,
        }
        if let Ok(entries) = serde_json::from_str::<Vec<Entry>>(trimmed) {
            let mut labels = BTreeSet::new();
            let mut depths = BTreeMap::new();
            for e in entries {
                let label = normalise_label(&e.label);
                if let Some(d) = e.target_distance.filter(|d| *d >= 0) {
                    depths.insert(label.clone(), d as u32);
                }
                labels.insert(label);
            }
            let depths = (!depths.is_empty()).then_some(depths);
            return (labels, depths);
        }
    }
    (
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(normalise_label)
            .collect(),
        None,
    )
}

/// [`parse_impacted`] without the distances.
pub fn parse_impacted_targets(text: &str) -> BTreeSet<String> {
    parse_impacted(text).0
}

/// Parses a `generate-hashes` file: JSON `{label: hash}`.
pub fn parse_hash_file(text: &str) -> Result<BTreeMap<String, String>> {
    serde_json::from_str(text).context("parsing a bazel-diff hash file")
}

/// The labels whose hash moved between two hash files — bazel-diff's own
/// `get-impacted-targets`, minus the transitive `depEdges` expansion.
///
/// Reimplemented here only so the comparison can be exercised (and the JSON
/// shape pinned) without a Bazel; the real runs always use the tool's own
/// implementation, which is what is being compared.
pub fn hash_diff(
    starting: &BTreeMap<String, String>,
    final_: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    final_
        .iter()
        .filter(|(label, hash)| starting.get(*label) != Some(*hash))
        .map(|(label, _)| label.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impacted_targets_parse_from_lines() {
        let out = "//a:b\n  //c:d  \n\n//e:f\n";
        let got = parse_impacted_targets(out);
        assert_eq!(
            got.into_iter().collect::<Vec<_>>(),
            vec!["//a:b", "//c:d", "//e:f"]
        );
    }

    #[test]
    fn impacted_targets_parse_from_the_dep_edges_json() {
        let out = r#"[{"label":"//a:b","targetDistance":0,"packageDistance":0},
                      {"label":"@@ext+//c:d","targetDistance":2,"packageDistance":1}]"#;
        let (got, depths) = parse_impacted(out);
        assert_eq!(
            got.into_iter().collect::<Vec<_>>(),
            vec!["//a:b", "@ext//c:d"],
            "the canonical external name is normalised"
        );
        let depths = depths.expect("targetDistance is a depth");
        assert_eq!(depths["@ext//c:d"], 2);
    }

    #[test]
    fn a_json_object_that_is_not_a_list_falls_back_to_lines() {
        // Defensive: an unexpected shape must not silently produce an empty
        // set, because an empty set reads as "the tools agree".
        let got = parse_impacted_targets("[not json\n//a:b\n");
        assert_eq!(
            got.into_iter().collect::<Vec<_>>(),
            vec!["//a:b", "[not json"]
        );
    }

    #[test]
    fn hash_files_diff_by_moved_and_new_labels() {
        let a = parse_hash_file(r#"{"//a:a":"1","//b:b":"2","//gone:g":"9"}"#).unwrap();
        let b = parse_hash_file(r#"{"//a:a":"1","//b:b":"3","//new:n":"4"}"#).unwrap();
        assert_eq!(
            hash_diff(&a, &b).into_iter().collect::<Vec<_>>(),
            vec!["//b:b", "//new:n"],
            "unchanged labels are out; moved and new labels are in"
        );
    }

    #[test]
    fn implementations_name_themselves() {
        assert_eq!(
            Implementation::parse("kotlin"),
            Some(Implementation::Kotlin)
        );
        assert_eq!(Implementation::parse("nope"), None);
        assert_eq!(Implementation::Rust.name(), "bazel-diff-rust");
    }

    #[test]
    fn the_version_comes_from_the_file_beside_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_version_file(dir.path()), None);
        std::fs::write(dir.path().join("VERSION"), "v46.1.0 27ce213\n").unwrap();
        assert_eq!(
            read_version_file(dir.path()).as_deref(),
            Some("v46.1.0 27ce213")
        );
    }

    #[test]
    fn a_missing_installation_says_where_it_looked() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(DIR_ENV, dir.path());
        std::env::remove_var(BIN_ENV);
        let err = BazelDiff::discover(Implementation::Rust)
            .unwrap_err()
            .to_string();
        std::env::remove_var(DIR_ENV);
        assert!(err.contains("bin/bazel-diff-rust"), "{err}");
        assert!(err.contains("fetch.sh"), "{err}");
    }
}
