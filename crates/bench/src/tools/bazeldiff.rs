//! The bazel-diff [`Tool`].
//!
//! bazel-diff answers the same question cambium does by a different route: it
//! runs one whole-repository `bazel query` per revision, hashes every target's
//! definition transitively into a `{label: hash}` JSON file, and diffs the two
//! files. That shape is why the comparison is worth making: it is the tool CI
//! systems actually use. The wall recorded is the cache-less one (hash base,
//! hash head, diff), the same cold question every peer is asked.
//!
//! Both implementations (the Rust binary and the Kotlin jar) take the same
//! flags, so they differ here only in how the process is spawned and in the
//! version string.
//!
//! # Flags
//!
//! `generate-hashes` is always run with **`-k` (`--keep_going`)**. On a
//! real repository at a real pin, `bazel query '//...:all-targets'` does not
//! load cleanly: `bazel/bazel` on macOS references a GraalVM distribution with
//! no `macos-x64` build and an RBE config that shells out to Docker, so the
//! query exits 3 and bazel-diff, without `-k`, fails the change set outright. With
//! `-k` it hashes everything that loaded — the same universe cambium indexes
//! with its own `--keep_going`, and the same universe the `rdeps` oracle sees
//! (see [`crate::oracles::rdeps`]). No `--useCquery`, no
//! `--fineGrainedHashExternalRepos` and no `--excludeExternalTargets`: each
//! changes what "affected" means, and on a real subject the external graph
//! bazel-diff hashes is one of the things the comparison measures.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context as _, Result};

use crate::tools::{
    normalise_label, run_capture, Install, RunContext, Tool, ToolLocations, ToolRun,
};
use crate::util::short;

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
    exec: Exec,
    version: String,
    name: String,
}

/// How `tools/bazel-diff/fetch.sh` lays the installation out.
const INSTALL: Install = Install {
    cache_name: "bazel-diff",
    binary: "bin/bazel-diff-rust",
    bin_env: BIN_ENV,
    dir_env: DIR_ENV,
    fetch_script: "tools/bazel-diff/fetch.sh",
};

/// The jar's place in the same installation.
const JAR: Install = Install {
    binary: "bazel-diff_deploy.jar",
    bin_env: JAR_ENV,
    ..INSTALL
};

/// The version fetch.sh recorded next to `bin/`, if it did.
///
/// Preferred over asking the binary: the Rust crate's own version lags the
/// repository tag (the tag-v46 build reports `bazel-diff 39.0.0`), and it is
/// the tag and commit that identify what was compared.
pub fn read_version_file(dir: &Path) -> Option<String> {
    Install::version_text(dir).map(|s| s.lines().collect::<Vec<_>>().join(" "))
}

impl BazelDiff {
    /// Locates the requested implementation from the environment
    /// (`$CAMBIUM_BAZEL_DIFF`, `$CAMBIUM_BAZEL_DIFF_JAR`,
    /// `$CAMBIUM_BAZEL_DIFF_DIR`), or explains where it looked.
    pub fn discover(imp: Implementation) -> Result<Self> {
        Self::discover_with(imp, &ToolLocations::from_env())
    }

    /// Locates the requested implementation where the command line said it
    /// is, or under the installation directory.
    pub fn discover_with(imp: Implementation, at: &ToolLocations) -> Result<Self> {
        let dir = INSTALL.dir(at.bazel_diff_dir.as_deref());
        let explicit = match imp {
            Implementation::Rust => at.bazel_diff.clone(),
            Implementation::Kotlin => at.bazel_diff_jar.clone(),
        };
        match explicit {
            Some(p) => Self::at(imp, &dir, p),
            None => Self::discover_in(imp, &dir),
        }
    }

    /// The installation under `dir`, as `tools/bazel-diff/fetch.sh` lays one
    /// out.
    pub fn discover_in(imp: Implementation, dir: &Path) -> Result<Self> {
        let path = match imp {
            Implementation::Rust => INSTALL.locate_in(dir)?,
            Implementation::Kotlin => JAR.locate_in(dir)?,
        };
        Self::at(imp, dir, path)
    }

    /// One implementation at an explicit path, with the version the
    /// installation `dir` records beside it.
    fn at(imp: Implementation, dir: &Path, path: PathBuf) -> Result<Self> {
        anyhow::ensure!(
            path.exists(),
            "no bazel-diff at {}; set ${}, or ${DIR_ENV}",
            path.display(),
            match imp {
                Implementation::Rust => BIN_ENV,
                Implementation::Kotlin => JAR_ENV,
            }
        );
        match imp {
            Implementation::Rust => {
                let version = read_version_file(dir)
                    .or_else(|| binary_version(&path))
                    .unwrap_or_else(|| "unknown".into());
                Ok(Self::new(imp, Exec::Binary(path), version))
            }
            Implementation::Kotlin => {
                let java = std::env::var_os("JAVA_HOME")
                    .map(|h| PathBuf::from(h).join("bin/java"))
                    .filter(|p| p.exists())
                    .unwrap_or_else(|| PathBuf::from("java"));
                let version = read_version_file(dir).unwrap_or_else(|| "unknown".into());
                Ok(Self::new(imp, Exec::Jar { java, jar: path }, version))
            }
        }
    }

    fn new(imp: Implementation, exec: Exec, version: String) -> Self {
        Self {
            exec,
            version,
            name: imp.name().to_string(),
        }
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
    /// The startup options are the run's, minus `--output_base`, so
    /// bazel-diff gets one output base per workspace directory rather than
    /// sharing cambium's — see [`RunContext::tool_startup_flags`] for what
    /// sharing it costs. `-k` always rides along; see the module docs for why.
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
            .arg(&ctx.bazel_binary)
            .arg("-k");
        let startup = ctx.tool_startup_flags();
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

impl BazelDiff {
    /// `get-impacted-targets` over two hash files.
    pub fn get_impacted_targets(
        &self,
        ws: &Path,
        starting: &Path,
        final_: &Path,
        out: &Path,
        ctx: &RunContext,
    ) -> Result<(BTreeSet<String>, Duration)> {
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

impl Tool for BazelDiff {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> String {
        self.version.clone()
    }

    fn affected(&self, ws: &Path, base: &str, head: &str, ctx: &RunContext) -> Result<ToolRun> {
        std::fs::create_dir_all(&ctx.scratch)?;
        let base_hashes = ctx.scratch_file(&format!("{}-{}.json", self.name, short(base, 12)));
        let head_hashes = ctx.scratch_file(&format!("{}-{}.json", self.name, short(head, 12)));
        let out = ctx.scratch_file(&format!(
            "{}-{}-{}.txt",
            self.name,
            short(base, 12),
            short(head, 12)
        ));

        // The base hash runs against the second worktree; the head hash
        // against cambium's own checkout, which is already at head.
        let base_wall = self
            .generate_hashes(&ctx.base_worktree, &base_hashes, ctx, &[])
            .with_context(|| format!("hashing {base} in {}", ctx.base_worktree.display()))?;
        let head_wall = self
            .generate_hashes(ws, &head_hashes, ctx, &[])
            .with_context(|| format!("hashing {head} in {}", ws.display()))?;
        let (labels, diff_wall) =
            self.get_impacted_targets(ws, &base_hashes, &head_hashes, &out, ctx)?;

        Ok(ToolRun {
            labels,
            cold_wall: base_wall + head_wall + diff_wall,
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
/// `{label, targetDistance, packageDistance}` whose labels are read and whose
/// distances are not.
///
/// Labels go through [`normalise_label`] so an external target has the same
/// spelling here as it does in Tango's and cambium's output; without that a
/// canonical-vs-apparent repository name puts one target on *both* sides of
/// the symmetric difference.
pub fn parse_impacted(text: &str) -> BTreeSet<String> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('[') {
        #[derive(serde::Deserialize)]
        struct Entry {
            label: String,
        }
        if let Ok(entries) = serde_json::from_str::<Vec<Entry>>(trimmed) {
            return entries.iter().map(|e| normalise_label(&e.label)).collect();
        }
    }
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(normalise_label)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impacted_targets_parse_from_lines() {
        let out = "//a:b\n  //c:d  \n\n//e:f\n";
        let got = parse_impacted(out);
        assert_eq!(
            got.into_iter().collect::<Vec<_>>(),
            vec!["//a:b", "//c:d", "//e:f"]
        );
    }

    #[test]
    fn impacted_targets_parse_from_the_dep_edges_json() {
        let out = r#"[{"label":"//a:b","targetDistance":0,"packageDistance":0},
                      {"label":"@@ext+//c:d","targetDistance":2,"packageDistance":1}]"#;
        let got = parse_impacted(out);
        assert_eq!(
            got.into_iter().collect::<Vec<_>>(),
            vec!["//a:b", "@ext//c:d"],
            "the canonical external name is normalised"
        );
    }

    #[test]
    fn a_json_object_that_is_not_a_list_falls_back_to_lines() {
        // Defensive: an unexpected shape must not silently produce an empty
        // set, because an empty set reads as "the tools agree".
        let got = parse_impacted("[not json\n//a:b\n");
        assert_eq!(
            got.into_iter().collect::<Vec<_>>(),
            vec!["//a:b", "[not json"]
        );
    }

    #[test]
    fn implementations_name_themselves() {
        assert_eq!(Implementation::Rust.name(), "bazel-diff-rust");
        assert_eq!(Implementation::Kotlin.name(), "bazel-diff-kotlin");
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
        let err = BazelDiff::discover_in(Implementation::Rust, dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("bin/bazel-diff-rust"), "{err}");
        assert!(err.contains("fetch.sh"), "{err}");

        let err = BazelDiff::discover_in(Implementation::Kotlin, dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("bazel-diff_deploy.jar"), "{err}");
    }
}
