//! The bazel-diff comparison stage, at the smallest preset.
//!
//! Skipped with a message when the bazel-diff binary has not been built, so a
//! clone without `tools/bazel-diff/fetch.sh` having run still tests green —
//! unless `CAMBIUM_REQUIRE_TOOLS=1`, which is what CI's matrix job sets right
//! after building the tool: there a skip would hide a broken fetch.

use cambium_bench::legacy::{self, LegacyOptions};
use cambium_bench::stages::StageContext;
use cambium_bench::tools::bazeldiff::{BazelDiff, Implementation};
use cambium_bench::tools::Tool as _;

#[test]
fn cambium_and_bazel_diff_agree_on_the_impacted_targets() {
    let bazel_diff = match BazelDiff::discover(Implementation::Rust) {
        Ok(b) => b,
        Err(e) if std::env::var_os("CAMBIUM_REQUIRE_TOOLS").is_some_and(|v| v == "1") => {
            panic!("CAMBIUM_REQUIRE_TOOLS is set and bazel-diff is missing: {e:#}")
        }
        Err(e) => {
            eprintln!("skipping: {e:#}");
            return;
        }
    };
    eprintln!("using {} {}", bazel_diff.name(), bazel_diff.version());

    let ctx = StageContext::new("tiny", 1).expect("tiny preset");
    let run = legacy::run(&ctx, &LegacyOptions::default())
        .expect("stage runs and the sets agree");

    // Something must have been impacted, or the comparison is vacuous.
    assert!(
        !run.agreed.is_empty(),
        "no agreed labels: {:?}",
        run.result.extra
    );
    assert!(run.bazel_diff_only.is_empty());
    // Every cambium-only label is an over-approximation, never a miss.
    assert!(run.agreed.len() + run.cambium_only.len() >= run.agreed.len());
    assert_ne!(run.bazel_diff_version, "");
    for key in [
        "bazel_diff_cold_ms",
        "bazel_diff_warm_ms",
        "cambium_ms",
        "agreed",
    ] {
        assert!(run.result.extra.contains_key(key), "missing {key}");
    }
}
