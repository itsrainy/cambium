//! Baseline artifact store (spec 8).
//!
//! Keys are `indexes/<lineage>/<tree>.bin`, keyed by git tree hash so commits
//! with identical content share one index, plus a `commits/<lineage>/<sha>`
//! pointer holding the tree for tooling that only has a sha. Lineage comes
//! first because node ids are only comparable within one, which makes a
//! lineage the unit you list, garbage-collect and retire; putting it in the
//! path means a store can be swept a lineage at a time with a prefix.
//!
//! Writes are idempotent. An index's payload is a pure function of the graph,
//! so two workers publishing the same commit write the same bytes, and each
//! backend either renames a complete temp file into place (directory) or
//! performs a whole-object PUT (HTTP, command). A reader therefore sees either
//! the previous object or a complete new one, never a partial write, and two
//! concurrent publishers of the same commit cannot produce a corrupt or
//! disagreeing artifact.
//!
//! Three backends cover the ground: a local directory for developer machines
//! and single-runner CI, HTTP GET/PUT for a plain object store, and a generic
//! command backend whose templates can drive `aws s3`, `gsutil`, `curl` or an
//! internal CLI without any Rust changes. `docs/STORE.md` has the exact
//! templates for S3 and GCS, and the design note for content-addressed blobs.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::StoreConfig;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no baseline store is configured")]
    NotConfigured,
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{op} failed for {name}: {detail}")]
    Backend {
        op: &'static str,
        name: String,
        detail: String,
    },
    #[error("store backend does not support {0}")]
    Unsupported(&'static str),
}

type Result<T> = std::result::Result<T, StoreError>;

/// A place published index files live.
pub trait ArtifactStore: Send + Sync {
    /// Copies `name` to `dest`. Returns false if the artifact is not present,
    /// which is an ordinary outcome (the ancestor walk asks for many).
    fn fetch(&self, name: &str, dest: &Path) -> Result<bool>;
    /// Publishes `src` under `name`.
    fn publish(&self, name: &str, src: &Path) -> Result<()>;
    /// Names currently held, if the backend can enumerate.
    fn list(&self) -> Result<Vec<String>>;
    /// Removes artifacts older than `retention_days`, never touching `keep`.
    /// Returns the number removed.
    fn gc(&self, _retention_days: u64, _keep: &str) -> Result<usize> {
        Err(StoreError::Unsupported("gc"))
    }
    fn describe(&self) -> String;
}

pub fn from_config(cfg: &StoreConfig) -> Option<Box<dyn ArtifactStore>> {
    match cfg {
        StoreConfig::None => None,
        StoreConfig::Dir { path } => Some(Box::new(DirStore::new(path.clone()))),
        StoreConfig::Http { url, headers } => {
            Some(Box::new(HttpStore::new(url.clone(), headers.clone())))
        }
        StoreConfig::Command {
            fetch_cmd,
            push_cmd,
            list_cmd,
        } => Some(Box::new(CommandStore {
            fetch_cmd: fetch_cmd.clone(),
            push_cmd: push_cmd.clone(),
            list_cmd: list_cmd.clone(),
        })),
    }
}

// ---------------------------------------------------------------- directory

pub struct DirStore {
    root: PathBuf,
}

impl DirStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

/// Walks a store directory, reporting index keys relative to `root`.
fn collect_keys(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(StoreError::Io {
                path: dir.to_path_buf(),
                source,
            })
        }
    };
    for e in rd.flatten() {
        let path = e.path();
        if path.is_dir() {
            collect_keys(root, &path, out)?;
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let Some(key) = rel.to_str() else { continue };
        let key = key.replace('\\', "/");
        let is_index = cambium_index::parse_index_key(&key).is_some()
            || cambium_index::parse_commit_pointer_key(&key).is_some()
            || (key.starts_with("cambium-index-") && key.ends_with(".bin"));
        if is_index {
            out.push(key);
        }
    }
    Ok(())
}

impl ArtifactStore for DirStore {
    fn fetch(&self, name: &str, dest: &Path) -> Result<bool> {
        let src = self.root.join(name);
        if !src.exists() {
            return Ok(false);
        }
        if src == dest {
            return Ok(true);
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::copy(&src, dest).map_err(|source| StoreError::Io {
            path: src.clone(),
            source,
        })?;
        Ok(true)
    }

    fn publish(&self, name: &str, src: &Path) -> Result<()> {
        let dest = self.root.join(name);
        let dir = dest.parent().unwrap_or(&self.root).to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|source| StoreError::Io {
            path: dir.clone(),
            source,
        })?;
        if src == dest {
            return Ok(());
        }
        // Write next to the target and rename, so a concurrent reader never
        // sees a half-written index and two concurrent publishers of the same
        // commit (writing identical bytes) cannot interleave.
        let tmp = dest.with_extension(format!(
            "bin.publishing.{}",
            cambium_index::format::temp_suffix()
        ));
        std::fs::copy(src, &tmp).map_err(|source| StoreError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, &dest).map_err(|source| StoreError::Io { path: dest, source })
    }

    fn list(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        // Keys are nested (`indexes/<lineage>/<commit>.bin`), so listing walks
        // the tree and reports keys relative to the root. Flat names from an
        // older store are still recognised, so an upgrade does not orphan
        // artifacts that are otherwise still usable.
        collect_keys(&self.root, &self.root, &mut out)?;
        out.sort();
        Ok(out)
    }

    fn gc(&self, retention_days: u64, keep: &str) -> Result<usize> {
        let cutoff = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(retention_days * 86_400))
            .unwrap_or(std::time::UNIX_EPOCH);
        let mut removed = 0;
        for name in self.list()? {
            if name == keep {
                continue;
            }
            let p = self.root.join(&name);
            let old = p
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t < cutoff)
                .unwrap_or(false);
            if old && std::fs::remove_file(&p).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn describe(&self) -> String {
        format!("dir:{}", self.root.display())
    }
}

// --------------------------------------------------------------------- http

pub struct HttpStore {
    base: String,
    headers: Vec<(String, String)>,
}

impl HttpStore {
    pub fn new(base: String, headers: Vec<String>) -> Self {
        let headers = headers
            .iter()
            .filter_map(|h| h.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        Self { base, headers }
    }

    fn url(&self, name: &str) -> String {
        format!("{}/{}", self.base.trim_end_matches('/'), name)
    }
}

impl ArtifactStore for HttpStore {
    fn fetch(&self, name: &str, dest: &Path) -> Result<bool> {
        let mut req = ureq::get(&self.url(name));
        for (k, v) in &self.headers {
            req = req.set(k, v);
        }
        match req.call() {
            Ok(resp) => {
                let mut out = Vec::new();
                resp.into_reader()
                    .read_to_end(&mut out)
                    .map_err(|source| StoreError::Io {
                        path: dest.to_path_buf(),
                        source,
                    })?;
                std::fs::write(dest, out).map_err(|source| StoreError::Io {
                    path: dest.to_path_buf(),
                    source,
                })?;
                Ok(true)
            }
            Err(ureq::Error::Status(404, _)) => Ok(false),
            Err(e) => Err(StoreError::Backend {
                op: "fetch",
                name: name.to_string(),
                detail: e.to_string(),
            }),
        }
    }

    fn publish(&self, name: &str, src: &Path) -> Result<()> {
        let body = std::fs::read(src).map_err(|source| StoreError::Io {
            path: src.to_path_buf(),
            source,
        })?;
        let mut req = ureq::put(&self.url(name));
        for (k, v) in &self.headers {
            req = req.set(k, v);
        }
        req.send_bytes(&body).map_err(|e| StoreError::Backend {
            op: "publish",
            name: name.to_string(),
            detail: e.to_string(),
        })?;
        Ok(())
    }

    fn list(&self) -> Result<Vec<String>> {
        Err(StoreError::Unsupported("list over plain HTTP"))
    }

    fn describe(&self) -> String {
        format!("http:{}", self.base)
    }
}

use std::io::Read;

// ------------------------------------------------------------------ command

/// Runs user-supplied commands. `{name}` and `{path}` are substituted in every
/// argument, so a template like
/// `["gsutil", "cp", "gs://bucket/{name}", "{path}"]` works unchanged.
pub struct CommandStore {
    fetch_cmd: Vec<String>,
    push_cmd: Vec<String>,
    list_cmd: Vec<String>,
}

impl CommandStore {
    fn run(
        cmd: &[String],
        name: &str,
        path: &Path,
        op: &'static str,
    ) -> Result<std::process::Output> {
        if cmd.is_empty() {
            return Err(StoreError::Unsupported(op));
        }
        let subst = |s: &String| {
            s.replace("{name}", name)
                .replace("{path}", &path.display().to_string())
        };
        let args: Vec<String> = cmd[1..].iter().map(subst).collect();
        let out = Command::new(subst(&cmd[0]))
            .args(&args)
            .output()
            .map_err(|e| StoreError::Backend {
                op,
                name: name.to_string(),
                detail: e.to_string(),
            })?;
        Ok(out)
    }
}

impl ArtifactStore for CommandStore {
    fn fetch(&self, name: &str, dest: &Path) -> Result<bool> {
        let out = Self::run(&self.fetch_cmd, name, dest, "fetch")?;
        if out.status.success() && dest.exists() {
            return Ok(true);
        }
        // A non-zero exit is how "not found" is reported by most CLIs; only
        // surface it as an error if the command could not run at all.
        tracing::debug!(
            name,
            status = ?out.status.code(),
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "fetch command reported no artifact"
        );
        Ok(false)
    }

    fn publish(&self, name: &str, src: &Path) -> Result<()> {
        let out = Self::run(&self.push_cmd, name, src, "publish")?;
        if out.status.success() {
            Ok(())
        } else {
            Err(StoreError::Backend {
                op: "publish",
                name: name.to_string(),
                detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }
    }

    fn list(&self) -> Result<Vec<String>> {
        let out = Self::run(&self.list_cmd, "", Path::new(""), "list")?;
        if !out.status.success() {
            return Err(StoreError::Backend {
                op: "list",
                name: String::new(),
                detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().rsplit('/').next().unwrap_or_default().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }

    fn describe(&self) -> String {
        format!(
            "command:{}",
            self.fetch_cmd.first().cloned().unwrap_or_default()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_store_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = DirStore::new(dir.path().join("store"));
        let src = dir.path().join("index.bin");
        std::fs::write(&src, b"payload").unwrap();

        store.publish("cambium-index-abc-123.bin", &src).unwrap();
        assert_eq!(store.list().unwrap(), vec!["cambium-index-abc-123.bin"]);

        let dest = dir.path().join("fetched.bin");
        assert!(store.fetch("cambium-index-abc-123.bin", &dest).unwrap());
        assert_eq!(std::fs::read(&dest).unwrap(), b"payload");
        assert!(!store.fetch("cambium-index-nope-000.bin", &dest).unwrap());
    }

    #[test]
    fn dir_store_gc_keeps_recent_and_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let store = DirStore::new(dir.path().to_path_buf());
        let src = dir.path().join("src.bin");
        std::fs::write(&src, b"x").unwrap();
        store.publish("cambium-index-a-1.bin", &src).unwrap();
        store.publish("cambium-index-b-2.bin", &src).unwrap();
        // Nothing is older than the retention window yet.
        assert_eq!(store.gc(30, "cambium-index-a-1.bin").unwrap(), 0);
        assert_eq!(store.list().unwrap().len(), 2);
        // With a zero-day window everything but the pinned artifact goes.
        assert_eq!(store.gc(0, "cambium-index-a-1.bin").unwrap(), 1);
        assert_eq!(store.list().unwrap(), vec!["cambium-index-a-1.bin"]);
    }

    #[test]
    fn command_store_substitutes_templates() {
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("cambium-index-x-1.bin");
        std::fs::write(&payload, b"hello").unwrap();
        let store = CommandStore {
            fetch_cmd: vec![
                "cp".into(),
                format!("{}/{{name}}", dir.path().display()),
                "{path}".into(),
            ],
            push_cmd: vec!["true".into()],
            list_cmd: vec!["ls".into(), dir.path().display().to_string()],
        };
        let dest = dir.path().join("out.bin");
        assert!(store.fetch("cambium-index-x-1.bin", &dest).unwrap());
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
        assert!(!store
            .fetch("missing.bin", &dir.path().join("no.bin"))
            .unwrap());
        assert!(store
            .list()
            .unwrap()
            .contains(&"cambium-index-x-1.bin".to_string()));
    }
}
