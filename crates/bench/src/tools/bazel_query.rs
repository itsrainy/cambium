//! `bazel query` under `--keep_going`, with one exit-3 policy.
//!
//! Every query the harness makes (the oracles and the classifier's rule
//! lookups) goes through here, so the flags and the exit code rule below have
//! one definition.
//!
//! **Why exit 3 is accepted.** Under `--keep_going` Bazel exits 3 when it
//! finished but hit errors, which on a real subject is routine: `bazel/bazel`
//! at its pin references external repositories that cannot be fetched on
//! macOS (a GraalVM distribution with no `macos-x64` build, an RBE config that
//! needs Docker), so the transitive closure of `//...` cannot be completed.
//! Bazel still prints everything that *did* load. Every caller here uses the
//! answer in a direction where a smaller one is weaker but never wrong -- an
//! oracle that says "cambium must contain all of this", a universe that sizes
//! cambium's own answer, a classifier predicate that must find evidence to
//! excuse a disagreement -- so the partial answer is kept and the caller is
//! told it was partial. Any other non-zero exit is a real failure.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, Result};
use cambium_bazel::Bazel;
use cambium_proto::blaze_query::{Rule, Target};

/// Exit code Bazel uses for "finished, with errors" under `--keep_going`.
const PARTIAL: i32 = 3;

/// The flags every benchmark query passes, whatever its output format.
///
/// No `--noimplicit_deps`, deliberately, and matching
/// `cambium_bazel::QUERY_FLAGS`: the index carries implicit edges, so an
/// oracle or a comparison that dropped them would be asking a different
/// question than the one it is checking.
const COMMON: &[&str] = &[
    "--order_output=no",
    "--keep_going",
    "--noshow_progress",
    "--ui_event_filters=-info",
];

/// One label per line, blank lines dropped.
pub fn parse_labels(out: &str) -> BTreeSet<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// `bazel query <expr> --output=label`, and whether the answer is partial.
///
/// `what` names the query in the error, since an expression can be a hundred
/// thousand characters of `rdeps` union.
pub fn labels(bazel: &Bazel, expr: &str, what: &str) -> Result<(BTreeSet<String>, bool)> {
    let a = answer(bazel, expr, what)?;
    Ok((a.labels, a.partial))
}

/// What one `--output=label` query said, and what it could not say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Answer {
    pub labels: BTreeSet<String>,
    /// Exit 3: finished with errors, the answer a lower bound.
    pub partial: bool,
    /// The labels Bazel reported it could not resolve, from stderr: a
    /// package or target that does not exist, or a repository whose fetch
    /// failed. See [`unresolved_labels`] for why this is the whole story of
    /// a partial answer.
    pub unresolved: BTreeSet<String>,
    /// Every pattern Bazel said it skipped, with the reason it gave
    /// ([`skipped_patterns`]).
    pub skipped: BTreeMap<String, String>,
}

/// `bazel query <expr> --output=label`, with what it could not resolve.
pub fn answer(bazel: &Bazel, expr: &str, what: &str) -> Result<Answer> {
    let out = bazel
        .command("query")
        .arg(expr)
        .arg("--output=label")
        .args(COMMON)
        .output()
        .with_context(|| format!("spawning bazel query {what}"))?;
    let partial = out.status.code() == Some(PARTIAL);
    let stderr = String::from_utf8_lossy(&out.stderr);
    anyhow::ensure!(
        out.status.success() || partial,
        "bazel query {what} failed with {}\n{}",
        out.status,
        super::tail(&stderr, 20)
    );
    Ok(Answer {
        labels: parse_labels(&String::from_utf8_lossy(&out.stdout)),
        partial,
        unresolved: if partial {
            unresolved_labels(&stderr)
        } else {
            BTreeSet::new()
        },
        skipped: if partial {
            skipped_patterns(&stderr)
        } else {
            BTreeMap::new()
        },
    })
}

/// The labels a `--keep_going` query said it could not resolve.
///
/// Why these matter more than the exit code: `bazel query` evaluates every
/// `select` branch, so one `@perfetto//:perfetto` that does not exist (envoy
/// names it under `//bazel:enable_perf_tracing`, and the module's overlay
/// `BUILD.bazel` shadows the `BUILD` that defines it) is an error on the
/// target that names it, and under `--keep_going` every target that
/// transitively depends on an erroring target is dropped from the universe.
/// On envoy that target is in `source/common/common`, so `rdeps(//..., x)`
/// loses most of the tree and reports a fraction of the truth as a lower
/// bound. The exit code says "partial"; these names say what was cut, and
/// they are the same list step after step, so a run's report can print them
/// once and a reader can judge the bound.
///
/// Three spellings, as Bazel prints them:
///
/// - `no such package '<label>': ...`
/// - `no such target '<label>': ...`
/// - `An error occurred during the fetch of repository '<canonical>':`
///
/// The first two carry a label; the third a canonical repository name, which
/// is recorded as `@@<name>//` so it sorts and reads like the others. The
/// `[unknown repo '<n>' requested from @@]` spelling inside a label is kept
/// as Bazel wrote it.
pub fn unresolved_labels(stderr: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in stderr.lines() {
        for marker in ["no such package '", "no such target '"] {
            if let Some(rest) = line.split_once(marker).map(|(_, r)| r) {
                // The label ends at the quote Bazel closes it with, which is
                // the one followed by `: ` and the reason; a label can carry
                // quotes of its own (`[unknown repo 'x' requested from @@]`).
                let end = rest.find("': ").or_else(|| rest.rfind('\''));
                if let Some(end) = end {
                    let label = &rest[..end];
                    if !label.is_empty() {
                        out.insert(label.to_string());
                    }
                }
            }
        }
        if let Some(rest) = line
            .split_once("An error occurred during the fetch of repository '")
            .map(|(_, r)| r)
        {
            if let Some(repo) = rest.split_once('\'').map(|(r, _)| r) {
                if !repo.is_empty() {
                    out.insert(format!("@@{repo}//"));
                }
            }
        }
    }
    out
}

/// The patterns a `--keep_going` query skipped, and why, from its
/// `Skipping '<pattern>': <reason>` lines.
///
/// A root the query was asked about and could not resolve is reported this
/// way, and the reason is where the difference lies. `no such target
/// '//b:x': target 'x' not declared in package 'b'` is a file no rule can
/// name (a patch, a README): nothing depends on it, and the empty answer is
/// the true one. Any other reason (`error loading package 'b': ...`, `no
/// such package '@@repo//': ...` for a package whose loads reach a
/// repository that will not fetch) means the root's package did not load,
/// and the answer for that root is not empty but unknown. Verbatim from
/// Bazel 8.7.0:
///
/// ```text
/// ERROR: Skipping '//b:undeclared.txt': no such target '//b:undeclared.txt': target 'undeclared.txt' not declared in package 'b' defined by /ws/b/BUILD; however, a source file of this name exists. ...
/// ERROR: Skipping '//a:src.txt': no such package '@@+_repo_rules+broken//': patch does not apply (simulated)
/// ```
pub fn skipped_patterns(stderr: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in stderr.lines() {
        let Some(rest) = line.split_once("Skipping '").map(|(_, r)| r) else {
            continue;
        };
        if let Some((pattern, why)) = rest.split_once("': ") {
            out.insert(pattern.to_string(), why.trim().to_string());
        }
    }
    out
}

/// Every rule proto `expr` yields, or `None` if the query could not be run at
/// all.
///
/// `--proto:flatten_selects=false` is the point of the call for the
/// classifier: flattened, an attribute's `select` branches are merged into one
/// list and the `selector_list` field is gone, so there is no way to tell a
/// `select`ed dep from an unconditional one.
///
/// `None` and an empty vector are different answers: nothing resolved is not
/// the same as the query not running, and the classifier caches the two
/// differently.
pub fn rules(bazel: &Bazel, expr: &str) -> Option<Vec<Rule>> {
    let out = bazel
        .command("query")
        .arg(expr)
        .args([
            "--output=streamed_proto",
            "--proto:flatten_selects=false",
            "--proto:locations=false",
        ])
        .args(COMMON)
        .output()
        .ok()?;
    if !out.status.success() && out.status.code() != Some(PARTIAL) {
        tracing::debug!(status = %out.status, "the rule query failed");
        return None;
    }
    let targets: Vec<Target> = cambium_proto::decode_all(out.stdout.as_slice()).ok()?;
    Some(targets.into_iter().filter_map(|t| t.rule).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_labels_are_read_from_the_three_spellings() {
        let err = "ERROR: /ws/source/common/common/BUILD:597:8: no such target '@@perfetto+//:perfetto': target 'perfetto' not declared in package ''\n\
                   ERROR: /ws/x/BUILD:1:1: no such package '@@[unknown repo 'chrusty' requested from @@]//cmd': The repository could not be resolved\n\
                   ERROR: /ob/external/bazel_tools/tools/build_defs/repo/http.bzl:159:45: An error occurred during the fetch of repository 'libpfm+':\n\
                   WARNING: Download from https://x failed\n\
                   ERROR: Evaluation of query \"rdeps(//..., set(//a:b))\" failed: errors were encountered while computing transitive closure\n";
        let u = unresolved_labels(err);
        assert_eq!(
            u.into_iter().collect::<Vec<_>>(),
            vec![
                "@@[unknown repo 'chrusty' requested from @@]//cmd".to_string(),
                "@@libpfm+//".to_string(),
                "@@perfetto+//:perfetto".to_string(),
            ]
        );
        assert!(unresolved_labels("INFO: nothing wrong\n").is_empty());
    }

    /// Verbatim from Bazel 8.7.0 (`rdeps(//..., set(...))` under
    /// `--keep_going`), the workspace path shortened to `/ws`.
    #[test]
    fn skipped_patterns_keep_the_reason() {
        let err = "ERROR: /ws/defs.bzl:2:9: An error occurred during the fetch of repository '+_repo_rules+broken':\n\
                   ERROR: no such package '@@+_repo_rules+broken//': patch does not apply (simulated)\n\
                   ERROR: Skipping '//a:src.txt': no such package '@@+_repo_rules+broken//': patch does not apply (simulated)\n\
                   ERROR: Skipping '//b:sub/x.txt': no such target '//b:sub/x.txt': target 'sub/x.txt' not declared in package 'b' defined by /ws/b/BUILD; however, a source file of this name exists.  (Perhaps add 'exports_files([\"sub/x.txt\"])' to b/BUILD?)\n";
        let s = skipped_patterns(err);
        assert_eq!(s.len(), 2);
        assert!(s["//a:src.txt"].starts_with("no such package '@@+_repo_rules+broken//'"));
        assert!(s["//b:sub/x.txt"].starts_with("no such target '//b:sub/x.txt'"));
    }

    #[test]
    fn labels_are_one_per_line_and_blank_lines_are_not_labels() {
        let set = parse_labels("//a:b\n\n  //c:d  \n\n");
        assert_eq!(set.len(), 2);
        assert!(set.contains("//a:b") && set.contains("//c:d"));
        assert!(parse_labels("").is_empty());
    }
}
