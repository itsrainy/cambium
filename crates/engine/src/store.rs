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

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::config::StoreConfig;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no index store is configured")]
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
    /// Whether this backend enumerates, which decides whether a publish also
    /// writes the lineage-less copies a blind fetch needs. A backend that
    /// always can says so without asking, so a publish into a store that
    /// does not exist yet does not first read it.
    fn listable(&self) -> bool {
        self.list().is_ok()
    }
    /// Removes artifacts older than `retention_days`, never touching `keep`.
    /// Returns the number removed.
    fn gc(&self, _retention_days: u64, _keep: &str) -> Result<usize> {
        Err(StoreError::Unsupported("gc"))
    }
    fn describe(&self) -> String;
}

/// The backend `cfg` names. A relative directory path is read against
/// `workspace`, the Bazel workspace root, never against the process's working
/// directory: `cambium.toml` is checked in at that root, and `cambi -C <dir>`
/// or a run from a subdirectory must reach the same store as one from the
/// root.
pub fn from_config(
    cfg: &StoreConfig,
    workspace: &Path,
    diags: &crate::diag::Diagnostics,
) -> Option<Box<dyn ArtifactStore>> {
    match cfg {
        StoreConfig::None => None,
        StoreConfig::Dir { path } => Some(Box::new(
            DirStore::new(workspace.join(path)).with_diagnostics(diags.clone()),
        )),
        StoreConfig::Http { url, headers } => {
            Some(Box::new(HttpStore::new(url.clone(), headers.clone())))
        }
        StoreConfig::Command {
            fetch_cmd,
            push_cmd,
            list_cmd,
            not_found_exit,
        } => Some(Box::new(CommandStore {
            fetch_cmd: fetch_cmd.clone(),
            push_cmd: push_cmd.clone(),
            list_cmd: list_cmd.clone(),
            not_found_exit: *not_found_exit,
            diags: diags.clone(),
            said_failed: std::sync::atomic::AtomicBool::new(false),
        })),
    }
}

/// A file being fetched into `dest`, written next to it and renamed into
/// place once complete, so a reader (or a second fetcher) never sees a
/// half-written index. Dropping it before `commit` removes the temp file.
struct PendingFetch {
    tmp: PathBuf,
    dest: PathBuf,
}

impl PendingFetch {
    fn start(dest: &Path) -> Result<(Self, File)> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let tmp = dest.with_extension(format!("fetching.{}", cambium_index::format::temp_suffix()));
        let file = File::create(&tmp).map_err(|source| StoreError::Io {
            path: tmp.clone(),
            source,
        })?;
        Ok((
            Self {
                tmp,
                dest: dest.to_path_buf(),
            },
            file,
        ))
    }

    fn commit(self) -> Result<()> {
        std::fs::rename(&self.tmp, &self.dest).map_err(|source| StoreError::Io {
            path: self.dest.clone(),
            source,
        })?;
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for PendingFetch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.tmp);
    }
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

// ---------------------------------------------------------------- directory

/// A directory of index files: a local path, or a shared mount.
///
/// What an I/O failure means here is one rule, the same on every read. A
/// root that does not exist is an empty store: a store nobody has published
/// to yet has no directory, and a fresh CI setup must build (or, under
/// `--no-full-build`, exit 4) rather than retry, so the read is a miss and
/// says so once as `CAMBI-W007`. Any other failure reaching the root or a
/// key under it (a file where a directory should be, a permission refused,
/// a mount that errors) is `StoreError::Io`, which is `CAMBI-E011` and exit
/// 5: the store is there and cannot be read, which a retry may fix and a
/// cold build would only hide.
pub struct DirStore {
    root: PathBuf,
    diags: crate::diag::Diagnostics,
    /// Whether the missing-root warning has been given, so a walk over many
    /// ancestors says it once.
    said_absent: std::sync::atomic::AtomicBool,
}

impl DirStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            diags: crate::diag::Diagnostics::new(),
            said_absent: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Reports into the engine's sink rather than into a buffer nobody reads.
    pub fn with_diagnostics(mut self, diags: crate::diag::Diagnostics) -> Self {
        self.diags = diags;
        self
    }

    /// Whether the root is there: `Ok(false)` when it does not exist (after
    /// saying so once), an error when it cannot be looked at or is not a
    /// directory.
    fn root_present(&self) -> Result<bool> {
        match std::fs::metadata(&self.root) {
            Ok(m) if m.is_dir() => Ok(true),
            Ok(_) => Err(StoreError::Io {
                path: self.root.clone(),
                source: std::io::Error::other("the store path is not a directory"),
            }),
            // A symlink whose target is missing is not a store nobody has
            // published to: a publish cannot create through it (the link
            // itself is in the way), and on a CI runner it is usually a cache
            // mount that did not come up.
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && std::fs::symlink_metadata(&self.root)
                        .is_ok_and(|m| m.file_type().is_symlink()) =>
            {
                Err(StoreError::Io {
                    path: self.root.clone(),
                    source: std::io::Error::other(
                        "the store path is a symlink to something that does not exist",
                    ),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.say_absent();
                Ok(false)
            }
            Err(source) => Err(StoreError::Io {
                path: self.root.clone(),
                source,
            }),
        }
    }

    fn say_absent(&self) {
        if self
            .said_absent
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        self.diags.emit(
            crate::diag::Diagnostic::warning(
                crate::diag::Code::W007StoreFetchFailed,
                format!(
                    "the store directory `{}` does not exist, so it holds no index; reading \
                     it as empty",
                    self.root.display()
                ),
            )
            .with_help(
                "expected until the first `--publish` creates it; otherwise check the path in \
                 [baseline].store (a relative path is read from the workspace root)",
            ),
        );
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
        // `exists()` would read every failure as absent, and a root that is a
        // file (ENOTDIR) or unreadable (EACCES) is not a miss.
        match std::fs::metadata(&src) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.root_present()?;
                return Ok(false);
            }
            Err(source) => return Err(StoreError::Io { path: src, source }),
        }
        if src == dest {
            return Ok(true);
        }
        let (pending, mut out) = PendingFetch::start(dest)?;
        let mut input = File::open(&src).map_err(io_err(&src))?;
        std::io::copy(&mut input, &mut out).map_err(io_err(&src))?;
        pending.commit()?;
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
        if !self.root_present()? {
            return Ok(out);
        }
        // Keys are nested (`indexes/<lineage>/<commit>.bin`), so listing walks
        // the tree and reports keys relative to the root. Flat names from an
        // older store are still recognised, so an upgrade does not orphan
        // artifacts that are otherwise still usable.
        collect_keys(&self.root, &self.root, &mut out)?;
        out.sort();
        Ok(out)
    }

    fn listable(&self) -> bool {
        true
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
    /// One agent for the store's lifetime: connection reuse across the
    /// ancestor walk's many GETs, and one place for the timeouts.
    agent: ureq::Agent,
}

/// Time to first byte and per-read stall before a store call is a failure.
/// Indexes are hundreds of megabytes, so this bounds silence, not the whole
/// transfer.
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

impl HttpStore {
    pub fn new(base: String, headers: Vec<String>) -> Self {
        let headers = headers
            .iter()
            .filter_map(|h| h.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(HTTP_TIMEOUT)
            .timeout_read(HTTP_TIMEOUT)
            .timeout_write(HTTP_TIMEOUT)
            .build();
        Self {
            base,
            headers,
            agent,
        }
    }

    fn url(&self, name: &str) -> String {
        format!("{}/{}", self.base.trim_end_matches('/'), name)
    }

    fn request(&self, method: &str, name: &str) -> ureq::Request {
        let mut req = self.agent.request(method, &self.url(name));
        for (k, v) in &self.headers {
            req = req.set(k, v);
        }
        req
    }
}

impl ArtifactStore for HttpStore {
    fn fetch(&self, name: &str, dest: &Path) -> Result<bool> {
        match self.request("GET", name).call() {
            Ok(resp) => {
                let (pending, mut out) = PendingFetch::start(dest)?;
                std::io::copy(&mut resp.into_reader(), &mut out).map_err(io_err(dest))?;
                pending.commit()?;
                Ok(true)
            }
            Err(ureq::Error::Status(404, _)) => Ok(false),
            // S3 and GCS answer 403 rather than 404 for a missing key when the
            // caller may not list the bucket, which is the usual shape of a
            // read-only CI credential. The ancestor walk asks for many keys
            // that do not exist, so this is a miss, not an outage. The cost:
            // a wrong credential reads as a miss too. A publisher finds out
            // (its PUT fails), but a read-only CI reader never publishes, so
            // for it a bad `headers` value is a cold build with nothing said
            // above `--log debug`. docs/STORE.md says so.
            Err(ureq::Error::Status(403, _)) => {
                tracing::debug!(name, "403 on GET; reading it as not found");
                Ok(false)
            }
            Err(e) => Err(StoreError::Backend {
                op: "fetch",
                name: name.to_string(),
                detail: e.to_string(),
            }),
        }
    }

    fn publish(&self, name: &str, src: &Path) -> Result<()> {
        let file = File::open(src).map_err(io_err(src))?;
        let len = file.metadata().map_err(io_err(src))?.len();
        self.request("PUT", name)
            .set("Content-Length", &len.to_string())
            .send(file)
            .map_err(|e| StoreError::Backend {
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

// ------------------------------------------------------------------ command

/// Runs user-supplied commands. `{name}` and `{path}` are substituted in every
/// argument, so a template like
/// `["gsutil", "cp", "gs://bucket/{name}", "{path}"]` works unchanged.
pub struct CommandStore {
    fetch_cmd: Vec<String>,
    push_cmd: Vec<String>,
    list_cmd: Vec<String>,
    /// See [`StoreConfig::Command::not_found_exit`].
    not_found_exit: Option<i32>,
    /// A fetch that failed with something on stderr is read as a miss, which
    /// turns into a cold build; that is worth a diagnostic and not only a log
    /// line, so the store carries the engine's sink.
    diags: crate::diag::Diagnostics,
    /// Whether that warning has been given. An outage fails every fetch of
    /// the ancestor walk the same way, so it is said once and the rest are
    /// logged.
    said_failed: std::sync::atomic::AtomicBool,
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
        let _ = std::fs::remove_file(dest);
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let code = out.status.code();
        match self.not_found_exit {
            // The command told us which exit means "no such object", so any
            // other failure is the store misbehaving, not a miss.
            Some(nf) if code != Some(nf) => Err(StoreError::Backend {
                op: "fetch",
                name: name.to_string(),
                detail: format!("exit {code:?}: {stderr}"),
            }),
            // A non-zero exit is how most CLIs report "not found", and the
            // ancestor walk asks for many keys that do not exist. But a
            // missing key is usually silent; a message on stderr may be an
            // expired credential or a wrong bucket, and reading that as "no
            // baseline" turns into a cold build with no explanation.
            _ => {
                if stderr.is_empty() {
                    tracing::debug!(name, status = ?code, "fetch command reported no artifact");
                } else if self
                    .said_failed
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    tracing::warn!(
                        name,
                        status = ?code,
                        %stderr,
                        "fetch command failed again; treating the artifact as absent"
                    );
                } else {
                    tracing::warn!(
                        name,
                        status = ?code,
                        %stderr,
                        "fetch command failed; treating the artifact as absent"
                    );
                    self.diags.emit(
                        crate::diag::Diagnostic::warning(
                            crate::diag::Code::W007StoreFetchFailed,
                            format!(
                                "the store's fetch command failed for `{name}` (exit \
                                 {code:?}) and said: {stderr}. Treating the artifact as \
                                 absent, which means a cold build rather than a splice"
                            ),
                        )
                        .with_help(
                            "check the credentials and the location in [baseline].store; said \
                             once per run, later failing fetches are logged at `--log warn`",
                        ),
                    );
                }
                Ok(false)
            }
        }
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
            // There is no key to name, so the message names the template,
            // and a command that failed silently still says how it exited.
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(StoreError::Backend {
                op: "list",
                name: "list_cmd".into(),
                detail: if stderr.is_empty() {
                    format!("exit {:?}, nothing on stderr", out.status.code())
                } else {
                    format!("exit {:?}: {stderr}", out.status.code())
                },
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(list_line_to_key)
            .collect())
    }

    fn describe(&self) -> String {
        format!(
            "command:{}",
            self.fetch_cmd.first().cloned().unwrap_or_default()
        )
    }
}

/// The store key on one line of `list_cmd` output, or `None` for a line that
/// is not one (a directory header, a blank).
///
/// Listings come in several shapes — `aws s3 ls --recursive` prints
/// `<date> <time> <size> <key>`, `gsutil ls -r` prints one full URL per line
/// with `<prefix>/:` headers, a plain `find` prints paths — and the key is
/// what comes after the store's own prefix. That prefix is not known here, but
/// every key starts with `indexes/` or `commits/`, so the key is the last
/// whitespace-separated token from its first such segment onward. A flat
/// `cambium-index-*.bin` name from the pre-lineage layout is still recognised
/// by its basename.
fn list_line_to_key(line: &str) -> Option<String> {
    let token = line.split_whitespace().last()?;
    if token.ends_with('/') || token.ends_with(':') {
        return None;
    }
    for marker in ["indexes/", "commits/"] {
        let at = if let Some(rest) = token.strip_prefix(marker) {
            Some(token.len() - rest.len() - marker.len())
        } else {
            token.find(&format!("/{marker}")).map(|i| i + 1)
        };
        if let Some(i) = at {
            return Some(token[i..].to_string());
        }
    }
    let base = token.rsplit('/').next().unwrap_or(token);
    (base.starts_with("cambium-index-") && base.ends_with(".bin")).then(|| base.to_string())
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
    fn dir_store_gc_goes_by_age_alone_and_a_republish_resets_the_age() {
        let dir = tempfile::tempdir().unwrap();
        let store = DirStore::new(dir.path().join("store"));
        let src = dir.path().join("src.bin");
        std::fs::write(&src, b"x").unwrap();
        let live = "indexes/aaaa/t1.bin";
        let other = "indexes/bbbb/t1.bin";
        let pointer = "commits/aaaa/c1";
        for k in [live, other, pointer] {
            store.publish(k, &src).unwrap();
        }
        let age = |k: &str| {
            let old = std::time::SystemTime::now() - Duration::from_secs(40 * 86_400);
            File::options()
                .write(true)
                .open(dir.path().join("store").join(k))
                .unwrap()
                .set_modified(old)
                .unwrap();
        };
        for k in [live, other, pointer] {
            age(k);
        }
        // The post-merge job publishes the live tree again: identical bytes,
        // a new file, a fresh age.
        store.publish(live, &src).unwrap();
        // Whatever the lineage, what is older than the window goes; the
        // pointer too.
        assert_eq!(store.gc(30, "").unwrap(), 2);
        assert_eq!(store.list().unwrap(), vec![live]);
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
            not_found_exit: None,
            diags: crate::diag::Diagnostics::new(),
            said_failed: std::sync::atomic::AtomicBool::new(false),
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

    #[test]
    fn list_lines_keep_the_whole_key_from_the_layout_root() {
        // `aws s3 ls --recursive`
        assert_eq!(
            list_line_to_key("2026-09-01 12:00:00    260000000 cambium/indexes/ab/cd.bin")
                .as_deref(),
            Some("indexes/ab/cd.bin")
        );
        // `gsutil ls -r`: full URLs, plus directory headers to skip.
        assert_eq!(
            list_line_to_key("gs://bucket/cambium/commits/ab/deadbeef").as_deref(),
            Some("commits/ab/deadbeef")
        );
        assert_eq!(list_line_to_key("gs://bucket/cambium/indexes/ab/:"), None);
        assert_eq!(list_line_to_key(""), None);
        // A plain relative listing.
        assert_eq!(
            list_line_to_key("indexes/by-tree/t1.bin").as_deref(),
            Some("indexes/by-tree/t1.bin")
        );
        // The flat pre-lineage layout.
        assert_eq!(
            list_line_to_key("bucket/cambium-index-a-1.bin").as_deref(),
            Some("cambium-index-a-1.bin")
        );
        assert_eq!(list_line_to_key("bucket/README.md"), None);
    }

    #[test]
    fn a_configured_not_found_exit_makes_other_failures_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = CommandStore {
            fetch_cmd: vec!["sh".into(), "-c".into(), "exit 2".into()],
            push_cmd: vec![],
            list_cmd: vec![],
            not_found_exit: Some(1),
            diags: crate::diag::Diagnostics::new(),
            said_failed: std::sync::atomic::AtomicBool::new(false),
        };
        assert!(store.fetch("x", &dir.path().join("x")).is_err());
        let store = CommandStore {
            fetch_cmd: vec!["sh".into(), "-c".into(), "exit 1".into()],
            push_cmd: vec![],
            list_cmd: vec![],
            not_found_exit: Some(1),
            diags: crate::diag::Diagnostics::new(),
            said_failed: std::sync::atomic::AtomicBool::new(false),
        };
        assert!(!store.fetch("x", &dir.path().join("x")).unwrap());
    }

    #[test]
    fn a_directory_store_whose_root_does_not_exist_is_empty_and_says_so_once() {
        let dir = tempfile::tempdir().unwrap();
        let diags = crate::diag::Diagnostics::new();
        let store =
            DirStore::new(dir.path().join("never-published")).with_diagnostics(diags.clone());
        assert_eq!(store.list().unwrap(), Vec::<String>::new());
        assert!(!store
            .fetch("indexes/a/b.bin", &dir.path().join("out.bin"))
            .unwrap());
        assert_eq!(store.list().unwrap(), Vec::<String>::new());
        let said = diags.drain();
        assert_eq!(
            said.len(),
            1,
            "once per store, however often it is asked: {said:?}"
        );
        assert_eq!(said[0].code, crate::diag::Code::W007StoreFetchFailed);
        assert!(
            said[0].message.contains("does not exist"),
            "{}",
            said[0].message
        );
        assert!(said[0].help.is_some());

        // A publish creates it, and says nothing on the way.
        let src = dir.path().join("src.bin");
        std::fs::write(&src, b"x").unwrap();
        let store = DirStore::new(dir.path().join("fresh")).with_diagnostics(diags.clone());
        assert!(store.listable());
        store.publish("indexes/a/b.bin", &src).unwrap();
        assert!(diags.is_empty(), "{:?}", diags.drain());
        assert_eq!(store.list().unwrap(), vec!["indexes/a/b.bin"]);
    }

    #[test]
    fn a_directory_store_that_cannot_be_read_is_an_io_error_not_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the root directory should be: ENOTDIR under it, the
        // `/dev/null/nope` shape.
        let file = dir.path().join("a-file");
        std::fs::write(&file, b"").unwrap();
        for root in [file.clone(), file.join("nope")] {
            let diags = crate::diag::Diagnostics::new();
            let store = DirStore::new(root.clone()).with_diagnostics(diags.clone());
            assert!(
                matches!(store.list(), Err(StoreError::Io { .. })),
                "{}: {:?}",
                root.display(),
                store.list()
            );
            assert!(
                matches!(
                    store.fetch("indexes/a/b.bin", &dir.path().join("out.bin")),
                    Err(StoreError::Io { .. })
                ),
                "{}",
                root.display()
            );
            assert!(diags.is_empty(), "an error is not also a warning");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_store_that_refuses_reading_is_an_io_error_not_a_miss() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("locked");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads everything regardless of the mode, so there is nothing
        // to observe there.
        let refused = std::fs::read_dir(&root).is_err();
        let store = DirStore::new(root.clone());
        let listed = store.list();
        let fetched = store.fetch("indexes/a/b.bin", &dir.path().join("out.bin"));
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        if refused {
            assert!(matches!(listed, Err(StoreError::Io { .. })), "{listed:?}");
            assert!(matches!(fetched, Err(StoreError::Io { .. })), "{fetched:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_store_behind_a_dangling_symlink_is_an_error_not_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("cache-mount");
        std::os::unix::fs::symlink(dir.path().join("not-mounted"), &link).unwrap();
        let diags = crate::diag::Diagnostics::new();
        let store = DirStore::new(link).with_diagnostics(diags.clone());
        assert!(matches!(store.list(), Err(StoreError::Io { .. })));
        assert!(matches!(
            store.fetch("indexes/a/b.bin", &dir.path().join("out.bin")),
            Err(StoreError::Io { .. })
        ));
        assert!(diags.is_empty(), "no W007 for it: {:?}", diags.drain());
    }

    #[test]
    fn a_command_store_says_a_failing_fetch_once_per_run() {
        let dir = tempfile::tempdir().unwrap();
        let diags = crate::diag::Diagnostics::new();
        let store = CommandStore {
            fetch_cmd: vec![
                "sh".into(),
                "-c".into(),
                "echo 'fatal error: Could not connect' >&2; exit 1".into(),
            ],
            push_cmd: vec![],
            list_cmd: vec![],
            not_found_exit: None,
            diags: diags.clone(),
            said_failed: std::sync::atomic::AtomicBool::new(false),
        };
        for k in [
            "indexes/by-tree/a.bin",
            "indexes/by-tree/b.bin",
            "commits/by-commit/c",
        ] {
            assert!(!store.fetch(k, &dir.path().join("out")).unwrap());
        }
        let said = diags.drain();
        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!(said[0].code, crate::diag::Code::W007StoreFetchFailed);
        assert!(
            said[0].message.contains("Could not connect"),
            "{}",
            said[0].message
        );
    }

    #[test]
    fn a_relative_directory_path_is_read_from_the_workspace_not_the_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let cfg = StoreConfig::Dir {
            path: ".cambium-store".into(),
        };
        let store = from_config(&cfg, &ws, &crate::diag::Diagnostics::new()).unwrap();
        assert_eq!(
            store.describe(),
            format!("dir:{}", ws.join(".cambium-store").display())
        );
        let abs = dir.path().join("elsewhere");
        let cfg = StoreConfig::Dir { path: abs.clone() };
        let store = from_config(&cfg, &ws, &crate::diag::Diagnostics::new()).unwrap();
        assert_eq!(store.describe(), format!("dir:{}", abs.display()));
    }

    #[test]
    fn a_failed_fetch_leaves_no_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = DirStore::new(dir.path().join("store"));
        let dest = dir.path().join("cache/out.bin");
        assert!(!store.fetch("indexes/a/b.bin", &dest).unwrap());
        assert!(!dest.exists());
        assert!(std::fs::read_dir(dir.path().join("cache"))
            .map(|d| d.count() == 0)
            .unwrap_or(true));
    }
}
