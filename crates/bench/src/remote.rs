//! The benchmark box, from a laptop: finding it, and pulling a subject's
//! results off it for an offline replay.
//!
//! What this does on the box is read-only: `scp` of result files and index
//! archives, and (for the paths cache) one `cambium-bench replay
//! --write-paths` over the mirror, which is `git diff` and nothing else.
//! Never Bazel, never a write into the results tree.
//!
//! Provisioning stays in `tools/ec2/launch.sh`; this is only what a replay
//! needs, and it shells out to `aws`, `ssh` and `scp` rather than linking a
//! cloud SDK for three commands.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result};

/// How to reach the box. Every field has the value `tools/bench/precheck.sh`
/// hard-coded; the flags on `pull-results` override them.
#[derive(Clone, Debug)]
pub struct BenchBox {
    /// An explicit host, instead of asking EC2.
    pub host: Option<String>,
    pub user: String,
    /// The ssh key.
    pub key: PathBuf,
    pub region: String,
    /// `Name=tag:<key>,Values=<values>` on `describe-instances`.
    pub tag_key: String,
    pub tag_values: String,
    /// The cambium checkout on the box, relative to the user's home.
    pub repo: String,
    /// The subjects (mirrors) directory on the box, relative to the home.
    pub subjects: String,
}

impl Default for BenchBox {
    fn default() -> Self {
        Self {
            host: None,
            user: "ubuntu".into(),
            key: crate::util::home().join(".ssh/cambium-corpus.pem"),
            region: "us-east-2".into(),
            tag_key: "cambium".into(),
            tag_values: "bench,corpus".into(),
            repo: "cambium".into(),
            subjects: ".cache/cambium/corpus".into(),
        }
    }
}

/// What one subject's pull found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pulled {
    /// The pull was skipped: the directory existed and the box's result files
    /// are the ones in it, or there was no box to ask.
    pub cached: bool,
    /// The directory existed with results the box no longer has, and was
    /// pulled again.
    pub refreshed: bool,
    /// `bases/*.bin` came over.
    pub bases: bool,
    /// `heads/*.bin` came over.
    pub heads: bool,
    /// The paths cache was written.
    pub paths: bool,
}

impl BenchBox {
    /// The running box's public DNS name: the explicit one, else the first
    /// running instance carrying the tag.
    pub fn host(&self) -> Result<Option<String>> {
        if let Some(h) = &self.host {
            return Ok(Some(h.clone()));
        }
        let out = Command::new("aws")
            .args(["ec2", "describe-instances", "--region", &self.region])
            .arg("--filters")
            .arg(format!(
                "Name=tag:{},Values={}",
                self.tag_key, self.tag_values
            ))
            .arg("Name=instance-state-name,Values=running")
            .args([
                "--query",
                "Reservations[0].Instances[0].PublicDnsName",
                "--output",
                "text",
            ])
            .output()
            .context("running aws ec2 describe-instances")?;
        if !out.status.success() {
            tracing::warn!(
                status = %out.status,
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "aws ec2 describe-instances failed"
            );
            return Ok(None);
        }
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok((!name.is_empty() && name != "None").then_some(name))
    }

    fn target(&self, host: &str) -> String {
        format!("{}@{host}", self.user)
    }

    /// `scp -q -i key [user@host:]<remote> <local>`; `Ok(false)` when scp
    /// failed, for the optional directories.
    fn scp(&self, host: &str, remote: &str, local: &Path) -> Result<bool> {
        let status = Command::new("scp")
            .args(["-q", "-i"])
            .arg(&self.key)
            .args(["-o", "StrictHostKeyChecking=accept-new"])
            .arg(format!("{}:{remote}", self.target(host)))
            .arg(local)
            .stderr(std::process::Stdio::null())
            .status()
            .context("running scp")?;
        Ok(status.success())
    }

    /// `ssh -i key user@host <command>`, with stdout captured.
    pub fn ssh(&self, host: &str, command: &str) -> Result<std::process::Output> {
        Command::new("ssh")
            .arg("-i")
            .arg(&self.key)
            .args(["-o", "StrictHostKeyChecking=accept-new"])
            .arg(self.target(host))
            .arg(command)
            .output()
            .context("running ssh")
    }

    /// Where a subject's results are on the box, relative to the home.
    fn results_of(&self, subject: &str) -> String {
        format!("{}/bench-results/{subject}", self.repo)
    }

    /// Pulls one subject into `<cache>/<subject>/`: the index at the pin, the
    /// results, the archived bases and heads, and (with `paths`) the diff
    /// of every recorded change set, derived on the box by the same binary.
    ///
    /// A subject already under `cache` is pulled again only when the box's
    /// result files differ from the cached ones, compared by sha256 over ssh,
    /// so a precheck never replays a run the box has since replaced. With no
    /// running box the cache is used as it is. Read-only on the box, and small: the largest
    /// index in the last full run was envoy's at 7.3 MB, the bases at
    /// most a few hundred megabytes a subject.
    pub fn pull(&self, subject: &str, cache: &Path, paths: bool) -> Result<Pulled> {
        let dir = cache.join(subject);
        let mut refreshed = false;
        if dir.is_dir() {
            let cached = Pulled {
                cached: true,
                ..Default::default()
            };
            let Some(host) = self.host()? else {
                tracing::warn!(%subject, "no running benchmark box to check the cached pull against; using it as it is");
                return Ok(cached);
            };
            let remote = self.remote_sums(&host, subject)?;
            if remote.is_empty() {
                tracing::warn!(%subject, "the box has no results for this subject; keeping the cached pull");
                return Ok(cached);
            }
            if remote == local_sums(&dir) {
                return Ok(cached);
            }
            tracing::info!(%subject, "the box's results differ from the cached pull; pulling again");
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("removing the stale pull {}", dir.display()))?;
            refreshed = true;
        }
        let host = self.host()?.with_context(|| {
            format!("no running benchmark box and nothing cached for {subject}")
        })?;
        std::fs::create_dir_all(dir.join("walk"))?;
        std::fs::create_dir_all(dir.join("bases"))?;
        std::fs::create_dir_all(dir.join("heads"))?;
        tracing::info!(%subject, %host, "pulling");
        let remote = self.results_of(subject);
        // The walk record is what a replay reads (`precheck::load_recorded`);
        // the rebuild oracle's rows beside it are tens of megabytes a subject
        // and nothing offline needs them, so they stay on the box.
        let required = [
            (format!("{remote}/index-*.bin"), dir.clone()),
            (format!("{remote}/walk/walk-*.json"), dir.join("walk")),
        ];
        for (from, to) in &required {
            if !self.scp(&host, from, to)? {
                let _ = std::fs::remove_dir_all(&dir);
                anyhow::bail!("could not copy {from} from {host}; nothing cached for {subject}");
            }
        }
        // The rest is optional: a run before the driver archived its bases
        // and the oracle its head has none of it, and scp failing on that
        // must not fail the pull.
        let bases = self.scp(&host, &format!("{remote}/bases/*.bin"), &dir.join("bases"))?;
        let heads = self.scp(&host, &format!("{remote}/heads/*.bin"), &dir.join("heads"))?;
        let paths = paths && self.pull_paths(&host, subject, &dir)?;
        Ok(Pulled {
            cached: false,
            refreshed,
            bases,
            heads,
            paths,
        })
    }

    /// sha256 of the subject's result files on the box, by path relative to
    /// its results directory: the same files a pull copies.
    fn remote_sums(&self, host: &str, subject: &str) -> Result<BTreeMap<String, String>> {
        let out = self.ssh(
            host,
            &format!(
                "cd ~/{} 2>/dev/null && sha256sum {RESULT_GLOBS} 2>/dev/null",
                self.results_of(subject)
            ),
        )?;
        Ok(parse_sums(&String::from_utf8_lossy(&out.stdout)))
    }

    /// The recorded change sets carry (base, head) and no diff: it was taken
    /// from the mirror at run time and never written down. This asks the box
    /// for it, once, with the box's own `cambium-bench replay --write-paths`
    /// over the mirror, printed to stdout and written beside the index here.
    fn pull_paths(&self, host: &str, subject: &str, dir: &Path) -> Result<bool> {
        let Some(name) = paths_file_name(dir) else {
            tracing::warn!(%subject, "no index pulled; cannot name the paths cache");
            return Ok(false);
        };
        let command = format!(
            "cd ~/{repo} && ./target/release/cambium-bench replay {subject} \
             --results-dir bench-results --repo ~/{subjects}/{subject}.git --write-paths -",
            repo = self.repo,
            subjects = self.subjects,
        );
        let out = self.ssh(host, &command)?;
        if !out.status.success() || out.stdout.is_empty() {
            tracing::warn!(
                %subject,
                status = %out.status,
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "could not derive the change-set diffs on the box; pass --repo by hand"
            );
            return Ok(false);
        }
        std::fs::write(dir.join(name), &out.stdout)?;
        Ok(true)
    }
}

/// The result files a pull copies, relative to a subject's results directory.
/// A pull cached from before the walk (`changes/`, `history/`) sums to
/// nothing here and is pulled again, which is the refresh it needs.
const RESULT_GLOBS: &str = "walk/walk-*.json";

/// sha256 of the cached result files, the same way the box is asked.
fn local_sums(dir: &Path) -> BTreeMap<String, String> {
    for tool in ["sha256sum", "shasum -a 256"] {
        let Ok(out) = Command::new("sh")
            .arg("-c")
            .arg(format!("{tool} {RESULT_GLOBS} 2>/dev/null"))
            .current_dir(dir)
            .output()
        else {
            continue;
        };
        let sums = parse_sums(&String::from_utf8_lossy(&out.stdout));
        if !sums.is_empty() {
            return sums;
        }
    }
    BTreeMap::new()
}

/// `sha256sum` output, `<hash>  <path>` (or `<hash> *<path>`), as path to hash.
pub(crate) fn parse_sums(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let (hash, path) = l.trim().split_once(char::is_whitespace)?;
            let path = path.trim_start().trim_start_matches('*');
            (hash.len() == 64 && !path.is_empty()).then(|| (path.to_string(), hash.to_string()))
        })
        .collect()
}

/// The paths cache is named after the index file beside it
/// (`index-<pin>[-<bazel version>].bin` -> `paths-<pin>[-<version>].json`):
/// the pin spelling carries the Bazel-version suffix, which is also the name
/// `cambium-bench replay` looks the cache up under.
pub(crate) fn paths_file_name(dir: &Path) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n.starts_with("index-") && n.ends_with(".bin"))
        .collect();
    names.sort();
    let first = names.into_iter().next()?;
    let pin = first
        .strip_prefix("index-")
        .and_then(|s| s.strip_suffix(".bin"))?;
    Some(format!("paths-{pin}.json"))
}

#[cfg(test)]
mod tests {
    use super::parse_sums;

    #[test]
    fn sums_read_both_spellings_and_skip_noise() {
        let h = "a".repeat(64);
        let sums = parse_sums(&format!(
            "{h}  walk/walk-x.json\n{h} *walk/walk-x-bazel_7.4.1.json\nsha256sum: nope: No such file\n"
        ));
        assert_eq!(sums.len(), 2);
        assert_eq!(sums["walk/walk-x.json"], h);
        assert!(sums.contains_key("walk/walk-x-bazel_7.4.1.json"));
    }

    use super::*;

    #[test]
    fn the_paths_cache_is_named_after_the_index_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(paths_file_name(dir.path()), None);
        std::fs::write(dir.path().join("index-abc123-7.4.1.bin"), "").unwrap();
        std::fs::write(dir.path().join("state.json"), "").unwrap();
        assert_eq!(
            paths_file_name(dir.path()).as_deref(),
            Some("paths-abc123-7.4.1.json")
        );
    }

    #[test]
    fn the_defaults_are_the_scripts_and_a_cached_subject_is_not_pulled() {
        let b = BenchBox::default();
        assert_eq!(b.user, "ubuntu");
        assert_eq!(b.region, "us-east-2");
        assert!(b.key.ends_with(".ssh/cambium-corpus.pem"));
        assert_eq!(b.results_of("envoy"), "cambium/bench-results/envoy");
        assert_eq!(b.target("h"), "ubuntu@h");
        let cache = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cache.path().join("envoy")).unwrap();
        // A host that resolves to nothing, so the test never asks EC2 and a
        // box that happens to be running cannot turn this into a real pull:
        // the sums come back empty and the cached pull is kept.
        let offline = BenchBox {
            host: Some("box.invalid".into()),
            ..BenchBox::default()
        };
        let p = offline.pull("envoy", cache.path(), true).unwrap();
        assert!(p.cached);
        // An explicit host is taken without asking EC2.
        let b = BenchBox {
            host: Some("box.example".into()),
            ..BenchBox::default()
        };
        assert_eq!(b.host().unwrap().as_deref(), Some("box.example"));
    }
}
