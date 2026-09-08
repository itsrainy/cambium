//! `bazel query` under `--keep_going`, with one exit-3 policy.
//!
//! Five places in the harness ran a query and each spelled out the same six
//! flags and the same "exit 3 is not a failure" rule, in four slightly
//! different ways -- one of them (`query_rule`) passed `--keep_going` and then
//! rejected the exit code it produces.
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

use std::collections::BTreeSet;

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
    let out = bazel
        .command("query")
        .arg(expr)
        .arg("--output=label")
        .args(COMMON)
        .output()
        .with_context(|| format!("spawning bazel query {what}"))?;
    let partial = out.status.code() == Some(PARTIAL);
    anyhow::ensure!(
        out.status.success() || partial,
        "bazel query {what} failed with {}\n{}",
        out.status,
        super::tail(&String::from_utf8_lossy(&out.stderr), 20)
    );
    Ok((parse_labels(&String::from_utf8_lossy(&out.stdout)), partial))
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
    fn labels_are_one_per_line_and_blank_lines_are_not_labels() {
        let set = parse_labels("//a:b\n\n  //c:d  \n\n");
        assert_eq!(set.len(), 2);
        assert!(set.contains("//a:b") && set.contains("//c:d"));
        assert!(parse_labels("").is_empty());
    }
}
