//! The benchmark box, from a laptop: finding it, and pulling a subject's
//! results off it for an offline replay.
//!
//! What this does on the box is read-only: `scp` of result files and index
//! archives, and — for the paths cache — one `cambium-bench replay
//! --write-paths` over the mirror, which is `git diff` and nothing else.
//! Never Bazel, never a write into the results tree.
//!
//! Provisioning stays in `tools/ec2/launch.sh`; this is only what a replay
//! needs, and it shells out to `aws`, `ssh` and `scp` rather than linking a
//! cloud SDK for three commands.

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
    /// The pull was skipped because the directory already existed.
    pub cached: bool,
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
    /// results, the archived bases and heads, and — with `paths` — the diff
    /// of every recorded change set, derived on the box by the same binary.
    ///
    /// A subject already under `cache` is not pulled again; delete its
    /// directory to refresh it. Read-only on the box, and small: the largest
    /// index in the last published run was envoy's at 7.3 MB, the bases at
    /// most a few hundred megabytes a subject.
    pub fn pull(&self, subject: &str, cache: &Path, paths: bool) -> Result<Pulled> {
        let dir = cache.join(subject);
        if dir.is_dir() {
            return Ok(Pulled {
                cached: true,
                ..Default::default()
            });
        }
        let host = self.host()?.with_context(|| {
            format!("no running benchmark box and nothing cached for {subject}")
        })?;
        std::fs::create_dir_all(dir.join("changes"))?;
        std::fs::create_dir_all(dir.join("history"))?;
        std::fs::create_dir_all(dir.join("bases"))?;
        std::fs::create_dir_all(dir.join("heads"))?;
        tracing::info!(%subject, %host, "pulling");
        let remote = self.results_of(subject);
        let required = [
            (format!("{remote}/index-*.bin"), dir.clone()),
            (format!("{remote}/changes/*.json"), dir.join("changes")),
        ];
        for (from, to) in &required {
            if !self.scp(&host, from, to)? {
                let _ = std::fs::remove_dir_all(&dir);
                anyhow::bail!("could not copy {from} from {host}; nothing cached for {subject}");
            }
        }
        // The rest is optional: a run published before the history wrote a
        // result, or before the driver archived its bases and the oracle its
        // head, has none of it, and scp failing on that must not fail the pull.
        let _ = self.scp(
            &host,
            &format!("{remote}/history/*.json"),
            &dir.join("history"),
        )?;
        let bases = self.scp(&host, &format!("{remote}/bases/*.bin"), &dir.join("bases"))?;
        let heads = self.scp(&host, &format!("{remote}/heads/*.bin"), &dir.join("heads"))?;
        let paths = paths && self.pull_paths(&host, subject, &dir)?;
        Ok(Pulled {
            cached: false,
            bases,
            heads,
            paths,
        })
    }

    /// The recorded change sets carry (base, head) and no diff — it was taken
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

/// The paths cache is named after the index file beside it
/// (`index-<pin>[-<bazel version>].bin` -> `paths-<pin>[-<version>].json`):
/// the pin spelling carries the Bazel-version suffix, which is also the name
/// `cambium-bench replay` looks the cache up under.
pub fn paths_file_name(dir: &Path) -> Option<String> {
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
        let p = b.pull("envoy", cache.path(), true).unwrap();
        assert!(p.cached);
        // An explicit host is taken without asking EC2.
        let b = BenchBox {
            host: Some("box.example".into()),
            ..BenchBox::default()
        };
        assert_eq!(b.host().unwrap().as_deref(), Some("box.example"));
    }
}
