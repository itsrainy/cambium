//! The small things every module needed a copy of.
//!
//! Nothing here is benchmark logic. It is the handful of one-liners that were
//! written out three and four times over — a percentile, the clock, the cache
//! directory a fetch script installs into, the filename a Bazel version is
//! stamped into — and drifted apart each time they were: two of the three
//! version-suffix builders escaped `/` and space, the third escaped `:` as
//! well.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::de::DeserializeOwned;

/// The `q`th percentile of an already-sorted sample, by nearest rank. Empty is
/// 0: a distribution over no runs has no shape, and the reports say `n/a`
/// beside it rather than reading the number.
pub fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() as f64 * q) as usize).min(sorted.len() - 1)]
}

/// Seconds since the epoch, or 0 if the clock is before it. A result file's
/// timestamp is for a reader, so a broken clock must not fail a run.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The short git sha of the cambium that was built, or `"unknown"`.
///
/// Stamped in at build time by `build.rs`, and *not* asked of `git` at run
/// time: a workload's working directory is a **subject's** checkout, so
/// `git rev-parse HEAD` there answers with envoy's HEAD, not ours. The two
/// call sites used to disagree about exactly that -- one ran `git` in the
/// process's cwd, the other in `CARGO_MANIFEST_DIR` -- so a history and a
/// stage report from the same binary could name two different cambiums.
pub fn tool_sha() -> &'static str {
    match option_env!("CAMBIUM_GIT_SHA") {
        Some(s) if !s.is_empty() => s,
        _ => "unknown",
    }
}

/// `$HOME`, or the system temp directory when there is none — a CI runner or a
/// daemon with no home still has to put a cache somewhere.
pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// `~/.cache/cambium/<name>`: where the `tools/*/fetch.sh` scripts install and
/// where the subjects, mirrors and stores live. Every caller lets an
/// environment variable win first; this is only the fallback.
pub fn cache_dir(name: &str) -> PathBuf {
    home().join(".cache/cambium").join(name)
}

/// `<stem>[-<version>]`, with the characters a Bazel version may carry but a
/// path component may not (`/`, a space, `:`) replaced by `_`.
///
/// Results and output bases are keyed by the Bazel that produced them, because
/// the same subject at the same pin answers differently under 7 and under 9.
/// An empty version is no version: `--bazel-version ''` means "whatever the
/// worktree asks for".
pub fn versioned_stem(stem: &str, version: Option<&str>) -> String {
    match version.filter(|v| !v.is_empty()) {
        Some(v) => format!("{stem}-{}", v.replace(['/', ' ', ':'], "_")),
        None => stem.to_string(),
    }
}

/// [`versioned_stem`] with an extension.
pub fn versioned_name(stem: &str, version: Option<&str>, ext: &str) -> String {
    format!("{}.{ext}", versioned_stem(stem, version))
}

/// Just the `schema` field, so a file can be refused before it is parsed into
/// a type whose fields have since been renamed.
#[derive(serde::Deserialize)]
struct SchemaProbe {
    #[serde(default)]
    schema: u32,
}

/// Reads a result file, refusing one this binary cannot read.
///
/// Every result carries [`crate::RESULT_SCHEMA`], and the whole point of the
/// field is that an older file is *not* silently accepted: serde's `default`
/// attributes would happily fill a renamed field with a zero and the report
/// would print it. The three readers each did a bare `from_str` instead, so
/// the version was written and never checked.
pub fn read_result<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let probe: SchemaProbe = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a result file", path.display()))?;
    anyhow::ensure!(
        probe.schema == crate::RESULT_SCHEMA,
        "{} is schema {} and this cambium reads {}; delete it and run the workload again",
        path.display(),
        probe.schema,
        crate::RESULT_SCHEMA
    );
    serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_percentile_is_the_nearest_rank_and_an_empty_sample_is_zero() {
        let s = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(percentile(&s, 0.5), 60);
        assert_eq!(percentile(&s, 0.9), 100);
        assert_eq!(percentile(&s, 1.0), 100, "q=1 does not run off the end");
        assert_eq!(percentile(&[], 0.5), 0);
        assert_eq!(percentile(&[7], 0.9), 7);
    }

    #[test]
    fn a_version_that_cannot_be_a_filename_is_escaped() {
        assert_eq!(versioned_name("index-abc", None, "bin"), "index-abc.bin");
        assert_eq!(
            versioned_name("index-abc", Some(""), "bin"),
            "index-abc.bin"
        );
        assert_eq!(
            versioned_name("history-abc", Some("7.x"), "json"),
            "history-abc-7.x.json"
        );
        assert_eq!(
            versioned_name("index-abc", Some("last/green"), "bin"),
            "index-abc-last_green.bin"
        );
        assert_eq!(
            versioned_name("changes-abc", Some("bazel 7.4.1"), "json"),
            "changes-abc-bazel_7.4.1.json"
        );
    }

    #[test]
    fn a_result_from_another_schema_is_refused_rather_than_half_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.json");

        std::fs::write(&path, r#"{"schema": 1, "repo": "bazel"}"#).unwrap();
        let e = read_result::<serde_json::Value>(&path)
            .unwrap_err()
            .to_string();
        assert!(e.contains("schema 1"), "{e}");

        // A file with no `schema` at all is from before the field existed,
        // which is just as unreadable.
        std::fs::write(&path, r#"{"repo": "bazel"}"#).unwrap();
        assert!(read_result::<serde_json::Value>(&path).is_err());

        std::fs::write(
            &path,
            format!(r#"{{"schema": {}, "repo": "bazel"}}"#, crate::RESULT_SCHEMA),
        )
        .unwrap();
        let v: serde_json::Value = read_result(&path).unwrap();
        assert_eq!(v["repo"], "bazel");
    }
}
